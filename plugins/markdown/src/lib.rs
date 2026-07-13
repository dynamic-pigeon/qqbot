use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};
use kovi_onebot::*;

const MAX_MARKDOWN_BYTES: usize = 32 * 1024;
const MARKDOWN_COOLDOWN: Duration = Duration::from_secs(10);
const COOLDOWN_ENTRY_TTL: Duration = Duration::from_secs(10 * 60);

static MARKDOWN_COOLDOWNS: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn check_cooldown(user_id: i64) -> Option<u64> {
    let mut entries = MARKDOWN_COOLDOWNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    entries.retain(|_, last| now.duration_since(*last) < COOLDOWN_ENTRY_TTL);
    if let Some(last) = entries.get(&user_id) {
        let elapsed = now.duration_since(*last);
        if elapsed < MARKDOWN_COOLDOWN {
            return Some((MARKDOWN_COOLDOWN - elapsed).as_secs().max(1));
        }
    }
    entries.insert(user_id, now);
    None
}

#[kovi::plugin]
async fn main() {
    help_msg::register_help("md", "根据 md 生成图片", "!md [md context]").await;

    plugin::on_msg(|event| async move {
        let msg = event.borrow_text().unwrap_or_default();

        let Some(md_content) = msg.strip_prefix("!md ") else {
            return;
        };
        if md_content.len() > MAX_MARKDOWN_BYTES {
            event.reply_and_quote(format!(
                "Markdown 内容过长，最大允许 {} KiB",
                MAX_MARKDOWN_BYTES / 1024
            ));
            return;
        }
        if let Some(remaining) = check_cooldown(event.user_id) {
            event.reply_and_quote(format!("请求过于频繁，请在 {remaining} 秒后重试"));
            return;
        }

        let img = match utils::md_to_img(md_content).await {
            Ok(data) => data,
            Err(err) => {
                tracing::warn!(user_id = event.user_id, "生成 Markdown 图片失败: {err}");
                event.reply_and_quote("生成图片失败，请稍后重试");
                return;
            }
        };

        let base64_img = base64::engine::general_purpose::STANDARD.encode(&img);
        let msg = Message::new().add_image(&format!("base64://{}", base64_img));
        event.reply_and_quote(msg);
    });
}
