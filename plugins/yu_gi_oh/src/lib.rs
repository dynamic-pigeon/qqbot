use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};
use tracing;

mod fetch_card;

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

        let card = match fetch_card::fetch_card(card_name).await {
            Ok(card) => card,
            Err(e) => {
                tracing::error!("Fetch card error: {}", e);
                event.reply(format!("❌ 查询卡片信息失败: {}", e));
                return;
            }
        };

        let img = match card.fetch_image().await {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Fetch card image error: {}", e);
                event.reply(format!("❌ 获取卡片图片失败: {}", e));
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
