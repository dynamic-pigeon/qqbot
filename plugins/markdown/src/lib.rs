use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};

#[kovi::plugin]
async fn main() {
    help_msg::register_help(
        "md".to_string(),
        "根据 md 生成图片".to_string(),
        Some("!md [md context]".to_string()),
    )
    .await;

    plugin::on_msg(|event| async move {
        let msg = event.borrow_text().unwrap_or_default();

        let Some(md_content) = msg.strip_prefix("!md ") else {
            return;
        };

        let img = match utils::md_to_img(md_content).await {
            Ok(data) => data,
            Err(err) => {
                event.reply(format!("Error generating image: {}", err));
                return;
            }
        };

        let base64_img = base64::engine::general_purpose::STANDARD.encode(&img);
        let msg = Message::new().add_image(&format!("base64://{}", base64_img));
        event.reply(msg);
    });
}
