use std::io::Cursor;
use std::time::Duration;

use base64::Engine as _;
use image::{ExtendedColorType, ImageEncoder, RgbImage, codecs::jpeg::JpegEncoder};
use kovi::{Message, RuntimeBot};
use kovi_onebot::{MessageRegistrar as _, OneBotMessage, OnebotTrait};
use utils::retry::retry_async_with_backoff;

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

pub(crate) fn image_message(text: Option<&str>, images: &[&[u8]]) -> Message {
    let mut message = match text {
        Some(text) => Message::new().add_text(text),
        None => Message::new(),
    };
    for bytes in images {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        message = message.add_image(&format!("base64://{encoded}"));
    }
    message
}

/// 按高度从中间横切成上下两半。整图发不出时，两半分开发。
fn split_image(bytes: &[u8]) -> Option<[Vec<u8>; 2]> {
    let source = image::load_from_memory(bytes).ok()?.to_rgb8();
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

pub(crate) async fn send_group_wait(
    bot: &RuntimeBot,
    group_id: i64,
    message: &Message,
) -> Result<(), SendFail> {
    send_wait(|| bot.send_group_msg_return(group_id, message.clone())).await
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
    header: String,
    hashes: &[String],
    originals: &[Vec<u8>],
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
        for bytes in originals {
            if let Some([first, second]) = split_image(bytes) {
                parts.push(first);
                parts.push(second);
            }
        }
        if !parts.is_empty() {
            let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
            let message = image_message(Some(&text), &refs);
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
