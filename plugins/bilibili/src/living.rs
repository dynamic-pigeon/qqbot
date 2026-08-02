use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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

/// 轮询防重入标记：上一轮还没跑完时直接跳过本轮。
static POLL_RUNNING: AtomicBool = AtomicBool::new(false);

/// 提前 return 时也能复位 [`POLL_RUNNING`]。
struct PollGuard;

impl Drop for PollGuard {
    fn drop(&mut self) {
        POLL_RUNNING.store(false, Ordering::Release);
    }
}

async fn scheduled_task(map: Arc<Mutex<HashMap<u64, bool>>>, bot: Arc<kovi::RuntimeBot>) {
    if POLL_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let _guard = PollGuard;

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

    // 只在比对状态时短暂持锁，网络请求和消息发送都在锁外进行
    let (start, end) = {
        let mut map = map.lock().await;
        // 响应缺失的 uid（已注销/封禁）不会再出现，直接清理，
        // 避免残留旧状态、等主播恢复后误报一次状态变更
        let present: HashSet<u64> = status.keys().copied().collect();
        map.retain(|uid, _| present.contains(uid));
        let mut start = Vec::new();
        let mut end = Vec::new();
        for (uid, info) in status {
            // 只有 live_status == 1 才算开播；2 是轮播（循环播放录像），不算直播
            let status = info.live_status == 1;
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
        (start, end)
    };

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
    // 无封面时跳过图片，避免对空 URL 做无意义的下载重试
    let img = if info.cover_from_user.is_empty() {
        Default::default()
    } else {
        match retry_async(async || fetch_img(&info.cover_from_user).await, 3).await {
            Ok(img) => img,
            Err(_) => {
                tracing::error!("获取直播封面图失败: {}", info.cover_from_user);
                Default::default()
            }
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
        // 用 send_group_msg_return 等待 onebot 确认送达，失败记日志，避免通知静默丢失
        if let Err(e) = bot.send_group_msg_return(*group, msg.clone()).await {
            tracing::warn!("直播通知发送失败 group={group}: {e}");
        }
    }
}

async fn fetch_img(url: &str) -> anyhow::Result<Bytes> {
    let bytes = crate::image::download_bili_image(
        url,
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
