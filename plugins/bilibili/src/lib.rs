use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::{
    Message, PluginBuilder as plugin,
    event::GroupMsgEvent,
    serde_json::{self, Value},
};

use crate::bv_parser::parse_url;

mod bv_parser;

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::REFERER,
        reqwest::header::HeaderValue::from_static("https://www.bilibili.com/"),
    );
    reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 Edg/147.0.0.0")
        .timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(16)
        .default_headers(headers)
        .build()
        .unwrap()
});

#[kovi::plugin]
async fn main() {
    plugin::on_group_msg(parse_bv);
}

async fn parse_bv(event: Arc<GroupMsgEvent>) {
    for msg in event.message.iter() {
        let bv_info = match msg.type_.as_str() {
            "json" => {
                let Some(obj) = msg
                    .data
                    .get("data")
                    .and_then(|data| data.as_str())
                    .and_then(|data| serde_json::from_str::<Value>(data).ok())
                else {
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

                parse_url(url).await.ok()
            }
            "text" => parse_url(msg.data["text"].as_str().unwrap()).await.ok(),
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
