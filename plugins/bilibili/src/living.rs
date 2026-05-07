use std::{
    collections::{HashMap, hash_map::Entry},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
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
        scheduled_task(map, bot)
    })
    .unwrap();
}

async fn scheduled_task(map: Arc<Mutex<HashMap<u64, bool>>>, bot: Arc<kovi::RuntimeBot>) {
    let mut map = map.lock().await;
    let cfg = config::read_config();

    let uids: Vec<u64> = cfg.subscribe.iter().map(|s| s.uid).collect();
    if uids.is_empty() {
        return;
    }
    let status = match fetch_living_status(&uids).await {
        Ok(status) => status,
        Err(e) => {
            tracing::error!("获取直播状态失败: {}", e);
            return;
        }
    };

    let mut start = Vec::new();
    let mut end = Vec::new();
    for (uid, info) in status {
        let status = info.live_status != 0;
        match map.entry(uid) {
            Entry::Vacant(e) => {
                e.insert(status);
            }
            Entry::Occupied(mut e) => {
                let prev = *e.get();
                if prev != status {
                    e.insert(status);
                    if status {
                        start.push((uid, info));
                    } else {
                        end.push((uid, info));
                    }
                }
            }
        }
    }

    for (uid, info) in start {
        let img = match fetch_img(&info.cover_from_user).await {
            Ok(img) => img,
            Err(_) => {
                tracing::error!("获取直播封面图失败: {}", info.cover_from_user);
                Default::default()
            }
        };
        let base64_img = STANDARD.encode(img);
        let mut msg = Message::new().add_text(format!(
            "{}正在直播【{}】\nhttps://live.bilibili.com/{}",
            info.uname, info.title, info.room_id
        ));
        if !base64_img.is_empty() {
            msg.push_image(&format!("base64://{}", base64_img));
        }

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
        let img = match fetch_img(&info.cover_from_user).await {
            Ok(img) => img,
            Err(_) => {
                tracing::error!("获取直播封面图失败: {}", info.cover_from_user);
                Default::default()
            }
        };
        let base64_img = STANDARD.encode(img);
        let mut msg = Message::new().add_text(format!("{}直播结束了", info.uname));
        if !base64_img.is_empty() {
            msg.push_image(&format!("base64://{}", base64_img));
        }

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

async fn fetch_img(url: &str) -> Result<Bytes, reqwest::Error> {
    let resp = CLIENT.get(url).send().await?;
    let bytes = resp.bytes().await?;
    Ok(bytes)
}

pub async fn check_uid(uid: u64) -> bool {
    let status = fetch_living_status(&[uid]).await;
    match status {
        Ok(status) => status.contains_key(&uid),
        Err(e) => {
            tracing::error!("检查 uid 是否存在失败: {}", e);
            false
        }
    }
}

pub async fn fetch_uid_names(uids: &[u64]) -> anyhow::Result<HashMap<u64, String>> {
    let status = fetch_living_status(uids).await?;
    Ok(status
        .into_iter()
        .map(|(uid, info)| (uid, info.uname))
        .collect())
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

    #[tokio::test]
    async fn test_check_uid() {
        let uid = 272925261;
        let exists = super::check_uid(uid).await;
        assert!(exists, "UID {} should exist", uid);
        let uid = 484415486;
        let exists = super::check_uid(uid).await;
        assert!(!exists, "UID {} should not exist", uid);
    }

    #[tokio::test]
    async fn test_fetch_uid_names() {
        let uids = [272925261, 2412572, 518817];
        let names = super::fetch_uid_names(&uids).await.unwrap();
        println!("{:#?}", names);
    }
}
