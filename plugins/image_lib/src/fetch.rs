use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use kovi::{Message, Segment, serde_json::Value};

pub const MAX_ADD_IMAGES: usize = 25;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// QQ 图床域名。只列图片 CDN，不用整个 `qq.com`，避免任意子域过白名单。
const ALLOWED_QQ_HOSTS: &[&str] = &[
    "multimedia.nt.qq.com.cn",
    "gchat.qpic.cn",
    "c2cpicdw.qpic.cn",
    "gtimg.cn",
    "qpic.cn",
];

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("无法读取引用的消息")]
    MessageUnavailable,
    #[error("引用的消息里没有图片")]
    NoImages,
    #[error("一次最多添加 {MAX_ADD_IMAGES} 张图")]
    TooManyImages,
    #[error("请只回复一张图")]
    ExpectSingleImage,
    #[error("单张图片不能超过 {limit_mib} MiB")]
    TooLarge { limit_mib: u64 },
    #[error("有图片未能读取")]
    Unreadable,
}

impl From<FetchError> for utils::command::CommandError {
    fn from(error: FetchError) -> Self {
        utils::command::CommandError::user(error.to_string())
    }
}

pub fn extract_reply_id(message: &Message) -> Option<i32> {
    for segment in message.iter() {
        if segment.kind != "reply" {
            continue;
        }
        let id = segment.data.get("id")?;
        if let Some(n) = id.as_i64() {
            return i32::try_from(n).ok();
        }
        if let Some(s) = id.as_str() {
            return s.parse().ok();
        }
    }
    None
}

pub fn parse_message_segments(data: &Value) -> Result<Vec<Segment>> {
    let message = data
        .get("message")
        .ok_or_else(|| anyhow!("get_msg 返回缺少 message"))?;
    kovi::serde_json::from_value(message.clone()).context("解析引用消息段失败")
}

pub fn image_segments(segments: &[Segment]) -> Vec<&Segment> {
    segments
        .iter()
        .filter(|segment| segment.kind == "image")
        .collect()
}

/// 先数图再下载，避免引用了几十张图时先把内存打满再报「太多」。
pub fn select_images(
    segments: &[Segment],
    max: usize,
    single: bool,
) -> Result<Vec<&Segment>, FetchError> {
    let images = image_segments(segments);
    if images.is_empty() {
        return Err(FetchError::NoImages);
    }
    if single && images.len() != 1 {
        return Err(FetchError::ExpectSingleImage);
    }
    if images.len() > max {
        return Err(FetchError::TooManyImages);
    }
    Ok(images)
}

pub async fn load_image_bytes(segment: &Segment) -> Result<Vec<u8>, FetchError> {
    if let Some(url) = image_url(segment) {
        return download_remote(&url).await;
    }
    if let Some(path) = local_file_path(segment) {
        return read_local_limited(&path).await;
    }
    Err(FetchError::Unreadable)
}

fn image_url(segment: &Segment) -> Option<String> {
    for key in ["url", "file"] {
        let raw = segment.data.get(key).and_then(Value::as_str)?;
        if let Some(url) = normalize_https_url(raw) {
            return Some(url);
        }
    }
    None
}

fn normalize_https_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let with_scheme = if let Some(rest) = raw.strip_prefix("//") {
        format!("https:{rest}")
    } else {
        raw.to_owned()
    };
    let mut parsed = reqwest::Url::parse(&with_scheme).ok()?;
    match parsed.scheme() {
        "http" => parsed.set_scheme("https").ok()?,
        "https" => {}
        _ => return None,
    }
    Some(parsed.into())
}

fn local_file_path(segment: &Segment) -> Option<PathBuf> {
    // OneBot 缓存图是本机绝对路径；拒绝相对路径和 `..`，避免把 file 字段当任意读文件。
    let raw = segment.data.get("file").and_then(Value::as_str)?;
    let path = raw.strip_prefix("file://").unwrap_or(raw);
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    Some(path.to_path_buf())
}

fn max_image_bytes() -> usize {
    crate::config::static_config().max_image_bytes()
}

fn too_large() -> FetchError {
    FetchError::TooLarge {
        limit_mib: crate::config::static_config().max_image_mib(),
    }
}

async fn download_remote(url: &str) -> Result<Vec<u8>, FetchError> {
    let bytes =
        utils::download_image_limited(url, ALLOWED_QQ_HOSTS, max_image_bytes(), DOWNLOAD_TIMEOUT)
            .await
            .map_err(map_download_error)?;
    ensure_image_bytes(&bytes)?;
    Ok(bytes)
}

fn map_download_error(error: anyhow::Error) -> FetchError {
    let text = error.to_string();
    if text.contains("超过大小上限") || text.contains("超过") {
        too_large()
    } else {
        FetchError::Unreadable
    }
}

async fn read_local_limited(path: &Path) -> Result<Vec<u8>, FetchError> {
    let max_bytes = max_image_bytes();
    let meta = kovi::tokio::fs::metadata(path)
        .await
        .map_err(|_| FetchError::Unreadable)?;
    if !meta.is_file() {
        return Err(FetchError::Unreadable);
    }
    if meta.len() > max_bytes as u64 {
        return Err(too_large());
    }
    let bytes = kovi::tokio::fs::read(path)
        .await
        .map_err(|_| FetchError::Unreadable)?;
    if bytes.len() > max_bytes {
        return Err(too_large());
    }
    ensure_image_bytes(&bytes)?;
    Ok(bytes)
}

fn ensure_image_bytes(bytes: &[u8]) -> Result<(), FetchError> {
    if is_supported_image(bytes) {
        Ok(())
    } else {
        Err(FetchError::Unreadable)
    }
}

fn is_supported_image(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(&[0x89, b'P', b'N', b'G'])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
        || bytes.starts_with(b"BM")
}

#[cfg(test)]
mod tests {
    use kovi::{Message, serde_json::json};
    use kovi_onebot::MessageRegistrar as _;

    use super::*;

    #[test]
    fn extracts_reply_id_from_string_or_number() {
        let from_str = Message::new().add_reply(42).add_text("删除");
        assert_eq!(extract_reply_id(&from_str), Some(42));

        let mut numeric = Message::new();
        numeric.push(Segment {
            kind: "reply".to_owned(),
            data: json!({ "id": 99 }),
        });
        assert_eq!(extract_reply_id(&numeric), Some(99));
        assert_eq!(extract_reply_id(&Message::new().add_text("删除")), None);
    }

    #[test]
    fn parses_get_msg_image_segments() {
        let data = json!({
            "message": [
                { "type": "text", "data": { "text": "hi" } },
                { "type": "image", "data": { "url": "https://gchat.qpic.cn/a.jpg" } },
                { "type": "image", "data": { "file": "https://multimedia.nt.qq.com.cn/x" } }
            ]
        });
        let segments = parse_message_segments(&data).unwrap();
        assert_eq!(image_segments(&segments).len(), 2);
    }

    #[test]
    fn upgrades_http_image_url() {
        let segment = Segment {
            kind: "image".to_owned(),
            data: json!({ "url": "http://gchat.qpic.cn/foo.jpg" }),
        };
        assert_eq!(
            image_url(&segment).as_deref(),
            Some("https://gchat.qpic.cn/foo.jpg")
        );
    }

    #[test]
    fn rejects_relative_and_parent_local_paths() {
        let relative = Segment {
            kind: "image".to_owned(),
            data: json!({ "file": "secret.png" }),
        };
        assert!(local_file_path(&relative).is_none());

        let parent = Segment {
            kind: "image".to_owned(),
            data: json!({ "file": "/tmp/../etc/passwd" }),
        };
        assert!(local_file_path(&parent).is_none());
    }

    #[test]
    fn select_images_rejects_empty_and_overflow() {
        let none: Vec<Segment> = Vec::new();
        assert!(matches!(
            select_images(&none, 10, false),
            Err(FetchError::NoImages)
        ));

        let many = vec![
            Segment {
                kind: "image".to_owned(),
                data: json!({}),
            },
            Segment {
                kind: "image".to_owned(),
                data: json!({}),
            },
        ];
        assert!(matches!(
            select_images(&many, 10, true),
            Err(FetchError::ExpectSingleImage)
        ));
        assert!(matches!(
            select_images(&many, 1, false),
            Err(FetchError::TooManyImages)
        ));
    }

    #[test]
    fn recognizes_common_image_headers() {
        assert!(is_supported_image(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_supported_image(b"GIF89a...."));
        assert!(!is_supported_image(b"%PDF"));
    }
}
