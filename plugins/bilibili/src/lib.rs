use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::{
    Message, PluginBuilder as plugin,
    event::GroupMsgEvent,
    serde_json::{self, Value},
};

use crate::bv_parser::parse_url;

mod bv_parser;

#[kovi::plugin]
async fn main() {
    plugin::on_group_msg(parse_bv);
}

async fn parse_bv(event: Arc<GroupMsgEvent>) {
    for msg in event.message.iter() {
        let bv_info = match msg.type_.as_str() {
            "json" => {
                let Value::Object(ref data) = msg.data else {
                    continue;
                };

                let Some(Value::String(data)) = data.get("data") else {
                    continue;
                };

                let Ok(obj) = serde_json::from_str::<Value>(data) else {
                    continue;
                };

                let Some(url) = obj
                    .get("meta")
                    .and_then(|meta| meta.get("detail_1"))
                    .and_then(|detail| detail.get("qqdocurl"))
                    .and_then(|url| url.as_str())
                else {
                    continue;
                };

                match parse_url(url).await {
                    Ok(info) => Some(info),
                    Err(_) => None,
                }
            }
            "text" => match parse_url(msg.data["text"].as_str().unwrap()).await {
                Ok(info) => Some(info),
                Err(_) => None,
            },
            _ => None,
        };

        let Some(bv_info) = bv_info else {
            continue;
        };

        let img_base64 = STANDARD.encode(&bv_info.pic);

        let msg = Message::new()
            .add_text(bv_info.title.as_str())
            .add_image(&format!("base64://{}", img_base64))
            .add_text(format!(
                "UP主：{}\n点赞：{} 投币：{}\n收藏：{} 观看：{}\n{}",
                bv_info.name,
                bv_info.like,
                bv_info.coin,
                bv_info.favorite,
                bv_info.view,
                bv_info.url
            ));

        event.reply(msg);
    }
}
