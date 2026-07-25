use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use futures::future::join_all;
use kovi::{Message, PluginBuilder as plugin, tokio};
use kovi_onebot::{EventRegistrar as _, event::GroupMsgEvent};
use utils::command::CommandRouter;

#[macro_use]
mod config;
mod db;
mod msg_rank;
pub mod ocr;
mod word_cloud;

const MAX_OCR_IMAGES_PER_MESSAGE: usize = 3;
const MAX_STORED_MESSAGE_BYTES: usize = 4 * 1024;

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
        // 空闲连接定时归还，避免 keep-alive 的 TLS 连接无限期挂起。
        .pool_idle_timeout(Duration::from_secs(90))
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

    CommandRouter::new("msg_rank", Arc::clone(&bot))
        .register(word_cloud::wordcloud_command(Arc::clone(&path)))
        .register(msg_rank::daily_rank_command())
        .install()
        .expect("注册发言排行与词云命令失败");

    plugin::on_group_msg(add_msg);
    word_cloud::init(Arc::clone(&bot), Arc::clone(&path))
        .await
        .unwrap();
}

async fn add_msg(event: Arc<GroupMsgEvent>) {
    let group = event.group_id;
    let user = event.user_id;

    if !config::read_config().notify_group.contains(&group) {
        return;
    }

    let text = truncate_utf8(get_text(&event.message).await, MAX_STORED_MESSAGE_BYTES);
    if text.trim().is_empty() {
        return;
    }

    if let Err(e) = db::add_msg(group, user, text) {
        tracing::error!("添加消息失败: {}", e);
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

async fn get_text(msg: &Message) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut ocr_tasks = Vec::new();
    let mut image_count = 0usize;

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
                if image_count >= MAX_OCR_IMAGES_PER_MESSAGE {
                    continue;
                }
                if let Some(url) = seg.data.get("url").and_then(|v| v.as_str()) {
                    image_count += 1;
                    let url = url.to_string();
                    let idx = parts.len();
                    parts.push(String::new()); // OCR 完成后回填
                    let task = kovi::spawn(async move { ocr::ocr(&url).await });
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

#[cfg(test)]
mod tests {
    use super::truncate_utf8;

    #[test]
    fn truncate_utf8_never_splits_a_character() {
        assert_eq!(truncate_utf8("ab中文".to_string(), 5), "ab中");
        assert_eq!(truncate_utf8("short".to_string(), 10), "short");
    }
}
