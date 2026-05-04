use std::{collections::HashMap, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::{Message, PluginBuilder as plugin, serde_json::json, tokio::sync::Mutex};
use serde::Deserialize;

use crate::{CLIENT, config};

const LIVING_STATUS_URL: &str =
    "https://api.live.bilibili.com/room/v1/Room/get_status_info_by_uids";

#[derive(Deserialize, Debug)]
struct Resp {
    code: i32,
    data: HashMap<u64, LiveRoom>,
}

#[derive(Debug, Deserialize)]
struct LiveRoom {
    uname: String,
    title: String,
    room_id: u64,
    // 0: 未直播, 1: 直播中, 2: 轮播中
    live_status: i32,
    // 直播封面图URL
    cover_from_user: String,
}

pub async fn init() {
    let bot = plugin::get_runtime_bot();
    let map = Arc::new(Mutex::new(HashMap::<u64, bool>::new()));
    plugin::cron("* */1 * * *", move || {
        let map = Arc::clone(&map);
        let bot = Arc::clone(&bot);
        async move {
            let mut map = map.lock().await;
            let cfg = config::read_config().clone();
            let uids: Vec<u64> = cfg.subscribe.iter().map(|s| s.uid).collect();
            if uids.is_empty() {
                return;
            }
            match fetch_living_status(&uids).await {
                Ok(status) => {
                    let mut start = Vec::new();
                    let mut end = Vec::new();
                    for (uid, info) in status {
                        let status = info.live_status != 0;
                        if !map.contains_key(&uid) {
                            map.insert(uid, status);
                            continue;
                        }
                        let prev = map.get(&uid).unwrap();
                        if *prev != status {
                            map.insert(uid, status);
                            if status {
                                start.push((uid, info));
                            } else {
                                end.push((uid, info));
                            }
                        }
                    }

                    for (uid, info) in start {
                        let img = CLIENT
                            .get(&info.cover_from_user)
                            .send()
                            .await
                            .unwrap()
                            .bytes()
                            .await
                            .unwrap();
                        let base64_img = STANDARD.encode(img);
                        let msg = Message::new()
                            .add_text(format!("{} 开始了直播\n{}", info.uname, info.title))
                            .add_image(&format!("base64://{}", base64_img))
                            .add_text(format!("https://live.bilibili.com/{}", info.room_id));

                        for group in cfg
                            .subscribe
                            .iter()
                            .filter(|s| s.uid == uid)
                            .flat_map(|s| &s.groups)
                        {
                            bot.send_group_msg(*group, msg.clone());
                        }
                    }

                    for (uid, info) in end {
                        let msg = Message::new()
                            .add_text(format!("{} 刚刚结束了直播\n{}", info.uname, info.title));

                        for group in cfg
                            .subscribe
                            .iter()
                            .filter(|s| s.uid == uid)
                            .flat_map(|s| &s.groups)
                        {
                            bot.send_group_msg(*group, msg.clone());
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("获取直播状态失败: {}", e);
                }
            }
        }
    })
    .unwrap();
}

async fn fetch_living_status(uids: &[u64]) -> anyhow::Result<HashMap<u64, LiveRoom>> {
    let client = reqwest::Client::new();
    let body = json!({
        "uids": uids,
    });
    let resp = client
        .post(LIVING_STATUS_URL)
        .json(&body)
        .send()
        .await?
        .json::<Resp>()
        .await?;

    if resp.code != 0 {
        anyhow::bail!("API error: code {}", resp.code);
    }

    Ok(resp.data)
}

#[cfg(test)]
mod tests {
    use kovi::tokio;

    #[tokio::test]
    async fn test_fetch_living_status() {
        let uids = [272925261, 2412572, 518817];
        let status = super::fetch_living_status(&uids).await.unwrap();
        println!("{:#?}", status);
    }
}
