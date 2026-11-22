//! 富媒体上传与 file_info 缓存。
//!
//! 官方发送图片是两步：先 `POST /v2/groups/{group_openid}/files`（或
//! `/v2/users/{openid}/files`）拿到 `file_info`，再以 msg_type=7 发送。
//! 上传支持远程 URL 代下载与二进制直传（file_data 为 base64，5MB 以内）。
//! file_info 带 ttl，同一张图重复发送时按内容键缓存复用。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::json;

use crate::token::TokenError;

pub enum MediaScope {
    Group(String),
    User(String),
}

impl MediaScope {
    fn upload_path(&self) -> String {
        match self {
            MediaScope::Group(openid) => format!("/v2/groups/{openid}/files"),
            MediaScope::User(openid) => format!("/v2/users/{openid}/files"),
        }
    }

    pub fn send_path(&self) -> String {
        match self {
            MediaScope::Group(openid) => format!("/v2/groups/{openid}/messages"),
            MediaScope::User(openid) => format!("/v2/users/{openid}/messages"),
        }
    }

    /// file_info 只能在同类型会话内使用，缓存键带上会话。
    fn cache_tag(&self) -> &str {
        match self {
            MediaScope::Group(_) => "g",
            MediaScope::User(_) => "u",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaSource {
    Url(String),
    Bytes {
        data: Vec<u8>,
        filename: String,
    },
    /// `file://` 本地路径，上传前读成字节。
    FilePath(String),
}

impl MediaSource {
    fn cache_key(&self) -> String {
        match self {
            MediaSource::Url(url) => format!("url:{url}"),
            MediaSource::Bytes { data, .. } => format!("bytes:{:016x}", fnv1a(data)),
            MediaSource::FilePath(_) => unreachable!("本地路径在缓存键构造前已展开为字节"),
        }
    }
}

/// 内容寻址缓存键用的 64 位 FNV-1a。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 官方接口会把数字返回成字符串，ttl 两种形态都收。
fn de_ttl<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Ttl {
        Number(u64),
        Text(String),
    }

    match Ttl::deserialize(deserializer)? {
        Ttl::Number(number) => Ok(number),
        Ttl::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

/// file_info 留 60 秒安全余量；ttl 为 0 表示可长期复用，按一小时计。
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const LONG_TTL_FALLBACK: Duration = Duration::from_secs(60 * 60);

/// 上传类请求的超时（官方 SDK 同款 120 秒），覆盖 client 默认 30 秒。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

pub struct MediaStore {
    inner: std::sync::Mutex<HashMap<String, (String, Instant)>>,
}

#[derive(Debug, thiserror::Error)]
#[error("富媒体上传失败: {message}")]
pub struct MediaError {
    pub message: String,
}

impl From<TokenError> for MediaError {
    fn from(error: TokenError) -> Self {
        MediaError {
            message: error.to_string(),
        }
    }
}

/// 嗅探图片 Content-Type；素材接口要求扩展名与声明一致。
pub(crate) fn sniff_content_type(data: &[u8]) -> (&'static str, &'static str) {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        ("image/png", "png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        ("image/jpeg", "jpg")
    } else if data.starts_with(b"GIF8") {
        ("image/gif", "gif")
    } else if data.len() >= 12 && &data[8..12] == b"WEBP" {
        ("image/webp", "webp")
    } else {
        ("image/png", "png")
    }
}

/// 嵌图引用：URL + 像素宽高。markdown 图片语法必须带宽高，
/// 否则渲染成 `[img]` 纯文本。
#[derive(Clone)]
pub(crate) struct MaterialRef {
    pub url: String,
    pub width: u32,
    pub height: u32,
}

/// 从图片字节嗅探像素尺寸（PNG / GIF / JPEG），webp 等少见格式返回 None。
pub(crate) fn sniff_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    // PNG：IHDR 固定在 16..24，大端。
    if data.len() >= 24 && data.starts_with(&[0x89, b'P', b'N', b'G']) {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        return Some((width, height));
    }
    // GIF：逻辑屏幕描述符在 6..10，小端。
    if data.len() >= 10 && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a")) {
        let width = u16::from_le_bytes([data[6], data[7]]) as u32;
        let height = u16::from_le_bytes([data[8], data[9]]) as u32;
        return Some((width, height));
    }
    jpeg_dimensions(data)
}

/// JPEG：逐段扫 marker，第一个 SOFn 段里是高宽（大端）。
fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut offset = 2;
    while offset + 9 < data.len() {
        if data[offset] != 0xFF {
            offset += 1;
            continue;
        }
        let marker = data[offset + 1];
        if marker == 0xD8 || (0xD0..=0xD7).contains(&marker) {
            offset += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[offset + 2], data[offset + 3]]));
        let is_frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_frame {
            let height = u32::from(u16::from_be_bytes([data[offset + 5], data[offset + 6]]));
            let width = u32::from(u16::from_be_bytes([data[offset + 7], data[offset + 8]]));
            return Some((width, height));
        }
        if marker == 0xDA {
            // 扫到扫描数据还没碰到帧头，结构异常。
            return None;
        }
        offset += 2 + length;
    }
    None
}

/// markdown 嵌图素材：本地图上传后拿 `qqbot-media://` 引用与像素尺寸。
/// 引用 90 天滑动有效，按内容缓存复用；单 bot 上限 100 张，
/// 满额时平台报错，由调用方回退为独立 media 消息。
pub(crate) async fn material_url(
    http: &reqwest::Client,
    token: &crate::token::TokenManager,
    api_base: &str,
    store: &MediaStore,
    source: &MediaSource,
) -> Result<MaterialRef, MediaError> {
    let resolved = match source {
        MediaSource::FilePath(path) => MediaSource::Bytes {
            data: kovi::tokio::fs::read(path).await.map_err(|e| MediaError {
                message: format!("读取 {path} 失败: {e}"),
            })?,
            filename: std::path::Path::new(path)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "image.png".to_owned()),
        },
        other => other.clone(),
    };
    let MediaSource::Bytes { data, .. } = resolved else {
        return Err(MediaError {
            message: "markdown 嵌图只接受本地图或公网 URL".to_owned(),
        });
    };

    let Some((width, height)) = sniff_dimensions(&data) else {
        return Err(MediaError {
            message: "无法嗅探图片尺寸，markdown 嵌图需要宽高".to_owned(),
        });
    };

    let cache_key = format!("material:bytes:{:016x}", fnv1a(&data));
    if let Some(cached) = store.lookup(&cache_key) {
        return Ok(MaterialRef {
            url: cached,
            width,
            height,
        });
    }

    let (content_type, extension) = sniff_content_type(&data);
    let file_name = format!("image.{extension}");
    let md5 = {
        use md5::Digest as _;
        let mut hasher = md5::Md5::new();
        hasher.update(&data);
        hasher.finalize()
    };
    let sha1 = {
        use sha1::Digest as _;
        let mut hasher = sha1::Sha1::new();
        hasher.update(&data);
        hasher.finalize()
    };
    let to_hex = |digest: &[u8]| {
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };

    let authorization = token.authorization().await?;
    // 1. 申请上传
    let prepare = http
        .post(format!("{api_base}/v2/media-images/prepare"))
        .header("Authorization", &authorization)
        .json(&serde_json::json!({
            "file_name": file_name,
            "file_size": data.len(),
            "content_type": content_type,
            "md5": to_hex(&md5),
            "sha1": to_hex(&sha1),
        }))
        .send()
        .await
        .map_err(|e| MediaError {
            message: e.to_string(),
        })?;
    let status = prepare.status();
    let text = prepare.text().await.map_err(|e| MediaError {
        message: e.to_string(),
    })?;
    if !status.is_success() {
        return Err(MediaError {
            message: format!("prepare HTTP {status}: {text}"),
        });
    }
    #[derive(Deserialize)]
    struct PrepareResponse {
        upload_id: String,
        presigned_url: String,
    }
    let parsed: PrepareResponse = serde_json::from_str(&text).map_err(|e| MediaError {
        message: format!("prepare 响应解析失败（{text}）: {e}"),
    })?;

    // 2. 预签名 PUT 直传（不能带 Authorization 头，COS 只认预签名）
    let put = http
        .put(&parsed.presigned_url)
        .timeout(UPLOAD_TIMEOUT)
        .header("Content-Type", content_type)
        .body(data.clone())
        .send()
        .await
        .map_err(|e| MediaError {
            message: format!("上传失败: {e}"),
        })?;
    let put_status = put.status();
    if !put_status.is_success() {
        let body = put.text().await.unwrap_or_default();
        return Err(MediaError {
            message: format!("上传 HTTP {put_status}: {body}"),
        });
    }

    // 3. 完成上传，拿 qqbot-media:// 引用
    let complete = http
        .post(format!("{api_base}/v2/media-images/complete"))
        .header("Authorization", authorization)
        .json(&serde_json::json!({
            "upload_id": parsed.upload_id,
            "md5": to_hex(&md5),
        }))
        .send()
        .await
        .map_err(|e| MediaError {
            message: e.to_string(),
        })?;
    let status = complete.status();
    let text = complete.text().await.map_err(|e| MediaError {
        message: e.to_string(),
    })?;
    if !status.is_success() {
        return Err(MediaError {
            message: format!("complete HTTP {status}: {text}"),
        });
    }
    #[derive(Deserialize)]
    struct CompleteResponse {
        url: String,
        #[serde(default)]
        ttl: u64,
    }
    let parsed: CompleteResponse = serde_json::from_str(&text).map_err(|e| MediaError {
        message: format!("complete 响应解析失败（{text}）: {e}"),
    })?;

    // 滑动 90 天有效，缓存按 7 天保守收口。
    let lifetime = if parsed.ttl == 0 {
        Duration::from_secs(7 * 24 * 60 * 60)
    } else {
        Duration::from_secs(parsed.ttl).saturating_sub(EXPIRY_MARGIN)
    };
    store.store(&cache_key, &parsed.url, lifetime);
    Ok(MaterialRef {
        url: parsed.url,
        width,
        height,
    })
}

impl MediaStore {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn upload(
        &self,
        http: &reqwest::Client,
        token: &crate::token::TokenManager,
        api_base: &str,
        scope: &MediaScope,
        source: MediaSource,
    ) -> Result<String, MediaError> {
        // 本地路径先读成字节，之后统一按 URL / 字节两种来源处理。
        let source = match source {
            MediaSource::FilePath(path) => {
                let data = kovi::tokio::fs::read(&path).await.map_err(|e| MediaError {
                    message: format!("读取 {path} 失败: {e}"),
                })?;
                let filename = std::path::Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "image.png".to_owned());
                MediaSource::Bytes { data, filename }
            }
            resolved => resolved,
        };

        let cache_key = format!("{}:{}", scope.cache_tag(), source.cache_key());
        if let Some(cached) = self.lookup(&cache_key) {
            return Ok(cached);
        }

        // srv_send_msg=false：只取 file_info 由发送接口发出，直发会占主动消息频次。
        let mut body = json!({ "file_type": 1, "srv_send_msg": false });
        match &source {
            MediaSource::Url(url) => {
                body["url"] = json!(url);
            }
            MediaSource::Bytes { data, filename } => {
                body["file_data"] = json!(BASE64.encode(data));
                body["file_name"] = json!(filename);
            }
            MediaSource::FilePath(_) => unreachable!("上一段已把本地路径展开为字节"),
        }

        let authorization = token.authorization().await?;
        let response = http
            .post(format!("{}{}", api_base, scope.upload_path()))
            .timeout(UPLOAD_TIMEOUT)
            .header("Authorization", authorization)
            .json(&body)
            .send()
            .await
            .map_err(|e| MediaError {
                message: e.to_string(),
            })?;

        let status = response.status();
        let text = response.text().await.map_err(|e| MediaError {
            message: e.to_string(),
        })?;
        if !status.is_success() {
            return Err(MediaError {
                message: format!("HTTP {status}: {text}"),
            });
        }

        #[derive(Deserialize)]
        struct UploadResponse {
            file_info: String,
            #[serde(default, deserialize_with = "de_ttl")]
            ttl: u64,
        }
        let parsed: UploadResponse = serde_json::from_str(&text).map_err(|e| MediaError {
            message: format!("响应解析失败（{text}）: {e}"),
        })?;

        let lifetime = if parsed.ttl == 0 {
            LONG_TTL_FALLBACK
        } else {
            Duration::from_secs(parsed.ttl).saturating_sub(EXPIRY_MARGIN)
        };
        self.store(&cache_key, &parsed.file_info, lifetime);
        Ok(parsed.file_info)
    }

    fn lookup(&self, key: &str) -> Option<String> {
        let mut cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((file_info, expire_at)) = cache.get(key) {
            if *expire_at > Instant::now() {
                return Some(file_info.clone());
            }
            cache.remove(key);
        }
        None
    }

    fn store(&self, key: &str, file_info: &str, lifetime: Duration) {
        let mut cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if cache.len() > 1024 {
            cache.retain(|_, (_, expire_at)| *expire_at > Instant::now());
        }
        cache.insert(
            key.to_owned(),
            (file_info.to_owned(), Instant::now() + lifetime),
        );
    }
}
