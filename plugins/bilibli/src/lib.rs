use std::sync::{Arc, LazyLock};

use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use dashmap::DashMap;
use kovi::{
    Message, PluginBuilder as plugin,
    event::GroupMsgEvent,
    log,
    serde_json::{self, Value},
};

use crate::bv_parser::parse_url;

mod bv_parser;

#[kovi::plugin]
async fn main() {
    plugin::on_group_msg(parse_bv);
}

pub struct BvParser {
    cache: moka::future::Cache<String, ()>,
}

impl BvParser {
    pub fn new() -> Self {
        Self {
            cache: moka::future::Cache::builder()
                .max_capacity(100)
                .time_to_live(std::time::Duration::from_secs(50))
                .build(),
        }
    }

    pub async fn parse(&self, url: &str) -> Result<Arc<bv_parser::BvInfo>> {
        let g = self
            .cache
            .entry_by_ref(url)
            .or_insert_with(async { () })
            .await;

        // 每个群每50秒只能解析一次同一个链接，防止和其他机器人死循环
        if !g.is_fresh() {
            anyhow::bail!("已经解析过了");
        }

        parse_url(url).await
    }
}

async fn parse_bv(event: Arc<GroupMsgEvent>) {
    static PARSER: LazyLock<DashMap<i64, BvParser>> = LazyLock::new(|| DashMap::new());
    let group_id = event.group_id;
    let parser = PARSER.entry(group_id).or_insert_with(|| BvParser::new());
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

                match parser.parse(url).await {
                    Ok(info) => Some(info),
                    Err(e) => {
                        log::error!("解析失败: {}", e);
                        None
                    }
                }
            }
            "text" => match parser.parse(msg.data["text"].as_str().unwrap()).await {
                Ok(info) => Some(info),
                Err(e) => {
                    log::error!("解析失败: {}", e);
                    None
                }
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
