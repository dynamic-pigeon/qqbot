use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use kovi::{Message, PluginBuilder as plugin, serde_json::json, tokio::sync::Mutex};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};
use serde::Deserialize;
use utils::retry::retry_async;

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
    cover_from_user: String,
}

pub async fn init() {
    let bot = plugin::get_runtime_bot();
    let map = Arc::new(Mutex::new(HashMap::<u64, bool>::new()));
    let map_ = Arc::clone(&map);
    plugin::cron("* */1 * * *", move || {
        let map = Arc::clone(&map);
        let bot = Arc::clone(&bot);
        scheduled_task(map, bot)
    })
    .unwrap();

    // 每天凌晨0点清理一次订阅列表，移除已取消订阅的uid，避免map无限增长
    plugin::cron("0 0 * * *", move || {
        let map = Arc::clone(&map_);
        async move {
            let mut map = map.lock().await;
            let cfg = config::read_config();
            let uids: HashSet<u64> = cfg.subscribe.iter().map(|s| s.uid).collect();
            map.retain(|&uid, _| uids.contains(&uid));
            tracing::info!("已清理直播订阅列表，当前订阅数: {}", map.len());
        }
    })
    .unwrap();
}

async fn scheduled_task(map: Arc<Mutex<HashMap<u64, bool>>>, bot: Arc<kovi::RuntimeBot>) {
    let mut map = match map.try_lock() {
        Ok(map) => map,
        Err(_) => return,
    };

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
        notify(&bot, &cfg, uid, &info, NotifyKind::Start).await;
    }

    for (uid, info) in end {
        notify(&bot, &cfg, uid, &info, NotifyKind::End).await;
    }
}

enum NotifyKind {
    Start,
    End,
}

async fn notify(
    bot: &kovi::RuntimeBot,
    cfg: &crate::config::Config,
    uid: u64,
    info: &LiveRoom,
    kind: NotifyKind,
) {
    let img = match retry_async(async || fetch_img(&info.cover_from_user).await, 3).await {
        Ok(img) => img,
        Err(_) => {
            tracing::error!("获取直播封面图失败: {}", info.cover_from_user);
            Default::default()
        }
    };
    let base64_img = STANDARD.encode(&img);

    let text = match kind {
        NotifyKind::Start => format!(
            "{}正在直播【{}】\nhttps://live.bilibili.com/{}",
            info.uname, info.title, info.room_id
        ),
        NotifyKind::End => format!("{}直播结束了", info.uname),
    };

    let mut msg = Message::new().add_text(text);
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

async fn fetch_img(url: &str) -> anyhow::Result<Bytes> {
    let bytes = utils::download_image_limited(
        url,
        crate::dynamics::ALLOWED_BILI_HOSTS,
        10 * 1024 * 1024,
        std::time::Duration::from_secs(10),
    )
    .await?;
    Ok(Bytes::from(bytes))
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
    let body = json!({
        "uids": uids,
    });
    let resp = CLIENT
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

    // 依赖公网网络 + B 站 API 实际响应，CI/本地都不稳定。
    // 手动回归时 `cargo test -- --ignored` 跑。
    #[tokio::test]
    #[ignore = "依赖公网 B 站 API，非本地/CI 环境跳过"]
    async fn test_check_uid() {
        let uid = 272925261;
        let exists = super::check_uid(uid).await;
        assert!(exists, "UID {} should exist", uid);
        let uid = 484415486;
        let exists = super::check_uid(uid).await;
        assert!(!exists, "UID {} should not exist", uid);
    }
}
