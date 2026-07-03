use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use futures::future::join_all;
use kovi::{Message, PluginBuilder as plugin, tokio};
use kovi_onebot::{EventRegistrar as _, event::GroupMsgEvent};

#[macro_use]
mod config;
mod db;
mod msg_rank;
pub mod ocr;
mod word_cloud;

/// 小写的十六进制编码。`hex::encode` 的极简内联实现，避免引入 hex crate。
#[inline]
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .pool_max_idle_per_host(16)
        .timeout(Duration::from_secs(10))
        // SSRF 防御：禁止跟随 redirect，防止 attacker 用公网域名 → 内网 IP 跳转
        // 绕过 URL host 白名单。
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
});

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    let path = Arc::new(bot.get_data_path());

    let config_path = path.join("config.json");
    if let Err(e) = config::init_config(config_path).await {
        tracing::error!("初始化配置失败: {e}");
        return;
    }

    let db_path = path.join("msg.db");
    if !db_path.exists() {
        tokio::fs::create_dir_all(db_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::File::create(&db_path).await.unwrap();
    }

    db::init_db(&db_path).await.unwrap();
    plugin::drop(|| async move { db::flush_on_shutdown().await });

    plugin::on_group_msg(add_msg);
    word_cloud::init().await.unwrap();
    msg_rank::init().await.unwrap();
}

async fn add_msg(event: Arc<GroupMsgEvent>) {
    let group = event.group_id;
    let user = event.user_id;

    let text = if config::read_config().notify_group.contains(&group) {
        get_text(&event.message).await
    } else {
        event.borrow_text().unwrap_or_default().to_string()
    };

    if let Err(e) = db::add_msg(group, user, text).await {
        tracing::error!("添加消息失败: {}", e);
    }
}

async fn get_text(msg: &Message) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut ocr_tasks = Vec::new();
    let check_dns = config::read_config().validate_image_url_dns;

    for seg in msg.iter() {
        match seg.kind.as_str() {
            "text" => {
                if let Some(text) = seg
                    .data
                    .get("text")
                    .and_then(|v| v.as_str())
                    .filter(|t| !t.is_empty())
                {
                    parts.push(text.to_string());
                }
            }
            "image" => {
                if let Some(url) = seg.data.get("url").and_then(|v| v.as_str()) {
                    let url = url.to_string();
                    let idx = parts.len();
                    parts.push(String::new()); // OCR 完成后回填
                    let task = kovi::spawn(async move { ocr::ocr(&url, check_dns).await });
                    ocr_tasks.push((idx, task));
                }
            }
            _ => {}
        }
    }

    if !ocr_tasks.is_empty() {
        let fills = join_all(ocr_tasks.into_iter().map(|(idx, task)| async move {
            let text = match task.await {
                Ok(Ok(text)) => text.to_string(),
                Ok(Err(e)) => {
                    tracing::error!("OCR 失败: {}", e);
                    String::new()
                }
                Err(e) => {
                    tracing::error!("OCR 任务失败: {}", e);
                    String::new()
                }
            };
            (idx, text)
        }))
        .await;

        for (idx, text) in fills {
            parts[idx] = text;
        }
    }

    parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
