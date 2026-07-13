use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};
use kovi_onebot::*;

mod fetch_card;

const MAX_CARD_NAME_BYTES: usize = 128;
const QUERY_COOLDOWN: Duration = Duration::from_secs(3);
static QUERY_TIMES: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn query_allowed(user_id: i64) -> bool {
    let mut times = QUERY_TIMES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    times.retain(|_, last| now.duration_since(*last) < Duration::from_secs(10 * 60));
    if times
        .get(&user_id)
        .is_some_and(|last| now.duration_since(*last) < QUERY_COOLDOWN)
    {
        return false;
    }
    times.insert(user_id, now);
    true
}

#[kovi::plugin]
async fn main() {
    help_msg::register_help(
        "游戏王查卡",
        "与游戏王卡片信息查询相关的命令",
        "/查卡 [卡片名称] - 查询卡片信息",
    )
    .await;

    plugin::on_msg(|event| async move {
        let text = event.borrow_text().unwrap_or_default();
        let text = text.trim();

        let Some(card_name) = text.strip_prefix("/查卡 ") else {
            return;
        };
        if card_name.is_empty() || card_name.len() > MAX_CARD_NAME_BYTES {
            event.reply("❌ 卡片名称为空或过长");
            return;
        }
        if !query_allowed(event.user_id) {
            event.reply("⏳ 查询过于频繁，请稍后重试");
            return;
        }

        let card = match fetch_card::fetch_card(card_name).await {
            Ok(card) => card,
            Err(e) => {
                tracing::error!("Fetch card error: {}", e);
                event.reply("❌ 查询卡片信息失败，请稍后重试");
                return;
            }
        };

        let img = match card.fetch_image().await {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Fetch card image error: {}", e);
                event.reply("❌ 获取卡片图片失败");
                event.reply(format!("{}", card));
                return;
            }
        };

        let base64_img = base64::engine::general_purpose::STANDARD.encode(img);
        let msg = Message::new()
            .add_image(&format!("base64://{}", base64_img))
            .add_text(format!("{}", card));

        event.reply(msg);
    });
}
