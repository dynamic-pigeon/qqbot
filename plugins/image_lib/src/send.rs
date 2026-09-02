use std::time::Duration;

use base64::Engine as _;
use kovi::{Message, RuntimeBot};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};
use utils::retry::retry_async_with_backoff;

const SEND_TIMEOUT: Duration = Duration::from_secs(60);
const SEND_RETRIES: usize = 2;
const HASH_PREFIX_CHARS: usize = 12;

#[derive(Debug)]
pub(crate) enum SendFail {
    Timeout,
    Api(String),
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

pub(crate) async fn send_group_wait(
    bot: &RuntimeBot,
    group_id: i64,
    message: &Message,
) -> Result<(), SendFail> {
    send_wait(|| bot.send_group_msg_return(group_id, message.clone())).await
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
    error: &SendFail,
) {
    let mut text = header;
    for hash in hashes {
        text.push('\n');
        text.push_str(hash.get(..HASH_PREFIX_CHARS).unwrap_or(hash));
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
    // send_private_msg_return 把参数原样 JSON 化，只有字符串会当成文本。
    if let Err(error) = send_wait(|| bot.send_private_msg_return(admin_id, text.clone())).await {
        tracing::warn!("图库失败哈希私聊主管理员失败: {error:?}");
    }
}
