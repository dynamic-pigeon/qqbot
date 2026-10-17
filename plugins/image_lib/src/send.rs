use std::io::Cursor;
use std::time::Duration;

use image::{ExtendedColorType, ImageEncoder, RgbImage, codecs::jpeg::JpegEncoder};
use kovi::serde_json::{Value, json};
use kovi::{Message, RuntimeBot};
use kovi_onebot::{MessageRegistrar as _, OneBotMessage, OnebotTrait};
use utils::retry::retry_async_with_backoff;

use crate::similar::decode_limited;

const SEND_TIMEOUT: Duration = Duration::from_secs(60);
const SEND_RETRIES: usize = 2;
const HASH_PREFIX_CHARS: usize = 12;
const SPLIT_JPEG_QUALITY: u8 = 85;

#[derive(Debug)]
pub(crate) enum SendFail {
    Timeout,
    Api(String),
}

fn hash_prefix(hash: &str) -> &str {
    hash.get(..HASH_PREFIX_CHARS).unwrap_or(hash)
}

/// 一张待发图片：消息段要用的 URI（`base64://` 或 `file://`）加上
/// 原始字节数。size 供消息打包预算使用，两种发图形态口径一致。
pub(crate) struct OutgoingImage {
    uri: String,
    size: usize,
}

impl OutgoingImage {
    /// 库内 blob。send-file 形态直接把绝对路径写进消息段，由 OneBot
    /// 端读盘上传，bot 不再把内容读进内存；默认形态读盘转 base64。
    /// 文件不在（刚被回收或盘坏）返回 None，调用方按图缺失处理。
    pub(crate) async fn from_blob(
        store: &crate::store::Store,
        group_id: i64,
        hash: &str,
    ) -> Option<Self> {
        #[cfg(feature = "send-file")]
        {
            let path = store.blob_path(group_id, hash).ok()?;
            let size = kovi::tokio::fs::metadata(&path).await.ok()?.len() as usize;
            let absolute = std::path::absolute(&path).unwrap_or(path);
            Some(Self {
                uri: format!("file://{}", absolute.display()),
                size,
            })
        }
        #[cfg(not(feature = "send-file"))]
        {
            let bytes = store.read_blob(group_id, hash).await.ok()?;
            Some(Self::from_bytes(bytes))
        }
    }

    /// 不在库里的派生字节图（发送失败降级时切开的上下两半），始终
    /// base64。
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        let size = bytes.len();
        Self {
            uri: utils::base64_image(&bytes),
            size,
        }
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// 测试专用：按指定体积构造，不分配内容，打包预算测试用。
    #[cfg(test)]
    pub(crate) fn from_size(size: usize) -> Self {
        Self {
            uri: String::new(),
            size,
        }
    }
}

pub(crate) fn image_message<'a>(
    text: Option<&str>,
    images: impl IntoIterator<Item = &'a OutgoingImage>,
) -> Message {
    let mut message = match text {
        Some(text) => Message::new().add_text(text),
        None => Message::new(),
    };
    for image in images {
        message = message.add_image(&image.uri);
    }
    message
}

/// 按高度从中间横切成上下两半。整图发不出时，两半分开发。
fn split_image(bytes: &[u8]) -> Option<[Vec<u8>; 2]> {
    let source = decode_limited(bytes)?.to_rgb8();
    let width = source.width();
    let height = source.height();
    if width == 0 || height < 2 {
        return None;
    }
    let mid = height / 2;
    let first = image::imageops::crop_imm(&source, 0, 0, width, mid).to_image();
    let second = image::imageops::crop_imm(&source, 0, mid, width, height - mid).to_image();
    Some([encode_jpeg(&first)?, encode_jpeg(&second)?])
}

fn encode_jpeg(image: &RgbImage) -> Option<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut buf, SPLIT_JPEG_QUALITY)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(buf.into_inner())
}

pub(crate) fn forward_node(name: &str, uin: i64, message: &Message) -> Value {
    let content = OneBotMessage::from(message.clone());
    json!({
        "type": "node",
        "data": {
            "name": name,
            "nickname": name,
            "uin": uin,
            "user_id": uin,
            "content": content,
        }
    })
}

pub(crate) async fn send_group_wait(
    bot: &RuntimeBot,
    group_id: i64,
    message: &Message,
) -> Result<(), SendFail> {
    send_wait(|| bot.send_group_msg_return(group_id, message.clone())).await
}

pub(crate) async fn send_group_forward_wait(
    bot: &RuntimeBot,
    group_id: i64,
    nodes: &[Value],
) -> Result<(), SendFail> {
    let params = json!({
        "group_id": group_id,
        "messages": nodes,
    });
    match send_api_wait(bot, "send_group_forward_msg", &params).await {
        Ok(()) => Ok(()),
        Err(SendFail::Timeout) => Err(SendFail::Timeout),
        Err(first) => match send_api_wait(bot, "send_forward_msg", &params).await {
            Ok(()) => Ok(()),
            Err(SendFail::Timeout) => Err(SendFail::Timeout),
            Err(_) => Err(first),
        },
    }
}

async fn send_api_wait(
    bot: &RuntimeBot,
    action: &'static str,
    params: &Value,
) -> Result<(), SendFail> {
    send_wait(|| {
        let request = bot.send_api_return(action, params.clone());
        async move { request.await.map(|_| 0) }
    })
    .await
}

async fn send_private_wait(
    bot: &RuntimeBot,
    user_id: i64,
    message: &Message,
) -> Result<(), SendFail> {
    // send_private_msg_return 把 T 原样 JSON 化；Message 的字段名是 kind，OneBot 要 type。
    let payload = OneBotMessage::from(message.clone());
    send_wait(|| bot.send_private_msg_return(user_id, payload.clone())).await
}

/// 明确的 OneBot 失败才走 utils 的退避重试；超时当作成功结束，避免重复发图。
async fn send_wait<F, Fut>(mut send: F) -> Result<(), SendFail>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<i32, kovi::bot::ApiReturn>> + Send,
{
    enum Outcome {
        Sent,
        Timeout,
    }

    match retry_async_with_backoff(
        || {
            let fut = send();
            async move {
                match kovi::tokio::time::timeout(SEND_TIMEOUT, fut).await {
                    Ok(Ok(_)) => Ok(Outcome::Sent),
                    Err(_) => Ok(Outcome::Timeout),
                    Ok(Err(error)) => Err(format!(
                        "status={} retcode={} message={:?} data={}",
                        error.status, error.retcode, error.message, error.data
                    )),
                }
            }
        },
        SEND_RETRIES,
        Duration::from_secs(1),
        Duration::from_secs(2),
    )
    .await
    {
        Ok(Outcome::Sent) => Ok(()),
        Ok(Outcome::Timeout) => Err(SendFail::Timeout),
        Err(detail) => Err(SendFail::Api(detail)),
    }
}

pub(crate) async fn report_send_fail(
    bot: &RuntimeBot,
    store: &crate::store::Store,
    group_id: i64,
    header: String,
    hashes: &[String],
    error: &SendFail,
) {
    let mut text = header;
    for hash in hashes {
        text.push('\n');
        text.push_str(hash_prefix(hash));
    }
    let detail = match error {
        SendFail::Timeout => "timeout",
        SendFail::Api(detail) => detail.as_str(),
    };
    tracing::warn!("图库发图失败 {detail} {text}");
    let Some(admin_id) = bot
        .get_main_admin()
        .ok()
        .and_then(|admin| admin.try_as_i64())
    else {
        tracing::warn!("无法解析主管理员");
        return;
    };

    if matches!(error, SendFail::Api(_)) {
        let mut parts = Vec::new();
        for hash in hashes {
            // 切开降级只在失败报告时才需要字节，此时再读盘
            let Some(bytes) = store.read_blob(group_id, hash).await.ok() else {
                continue;
            };
            if let Some([first, second]) = split_image(&bytes) {
                parts.push(OutgoingImage::from_bytes(first));
                parts.push(OutgoingImage::from_bytes(second));
            }
        }
        if !parts.is_empty() {
            let message = image_message(Some(&text), &parts);
            match send_private_wait(bot, admin_id, &message).await {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!("图库失败切开私聊主管理员失败: {error:?}");
                }
            }
        }
    }

    if let Err(error) = send_wait(|| bot.send_private_msg_return(admin_id, text.clone())).await {
        tracing::warn!("图库失败哈希私聊主管理员失败: {error:?}");
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "send-file")]
    use kovi::tokio;

    use super::*;

    #[test]
    fn forward_node_uses_onebot_type_not_kind() {
        let message = image_message(Some("重复 1/2 · 约 90%"), &[]);
        let node = forward_node("重复 1/2 · 约 90%", 10000, &message);
        assert_eq!(node["type"], "node");
        assert_eq!(node["data"]["name"], "重复 1/2 · 约 90%");
        assert_eq!(node["data"]["uin"], 10000);
        assert_eq!(node["data"]["content"][0]["type"], "text");
        assert!(node["data"]["content"][0].get("kind").is_none());
    }

    #[test]
    fn bytes_image_is_base64_with_raw_size() {
        let image = OutgoingImage::from_bytes(vec![1, 2, 3]);
        assert_eq!(image.size(), 3);
        let message = image_message(None, &[image]);
        let file = message
            .iter()
            .find(|segment| segment.kind == "image")
            .and_then(|segment| segment.data.get("file"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(file.starts_with("base64://"));
    }

    #[cfg(feature = "send-file")]
    #[kovi::tokio::test]
    async fn blob_image_is_file_uri_with_disk_size() {
        let dir = std::env::temp_dir().join(format!("image_lib_send_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::store::Store::open_with_quota(
            dir.clone(),
            crate::config::DEFAULT_MAX_GROUP_MIB * 1024 * 1024,
        )
        .unwrap();
        let bytes = b"\x89PNGfile-form".to_vec();
        let hash = utils::sha256_hex(&bytes);
        let blobs = store.blobs_dir(1);
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join(&hash), &bytes).unwrap();

        let image = OutgoingImage::from_blob(&store, 1, &hash).await.unwrap();
        assert_eq!(image.size(), bytes.len());
        let message = image_message(None, &[image]);
        let file = message
            .iter()
            .find(|segment| segment.kind == "image")
            .and_then(|segment| segment.data.get("file"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(file.starts_with("file://"));
        assert!(file.ends_with(&format!("blobs/{hash}")));
        assert!(std::path::Path::new(&file["file://".len()..]).is_absolute());

        // blob 已不在盘上时按图缺失处理
        std::fs::remove_file(blobs.join(&hash)).unwrap();
        assert!(OutgoingImage::from_blob(&store, 1, &hash).await.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
