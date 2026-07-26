mod browser;
mod fetch;
mod types;

pub use fetch::fetch_user_dynamics;
pub use types::{DynamicAuthor, DynamicItem, Pic, RichText};

/// B 站动态 / 封面图片域名白名单，用于 `validate_image_url_async` 的 SSRF 防御。
pub const ALLOWED_BILI_HOSTS: &[&str] = &["bilibili.com", "hdslb.com"];

/// 启动期触发预解析 SPACE_FEED_URL，确保硬编码 URL 被损坏时立即 panic
/// （而不是等到第一次 /dynamic fetch / cron 才暴露）。
pub fn warm_up() {
    use std::ops::Deref;
    let _ = fetch::SPACE_FEED_URL_PARSED.deref();
}

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use kovi::{Message, PluginBuilder as plugin, RuntimeBot};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};

use crate::config;

static POLL_LOCK: kovi::tokio::sync::Mutex<()> = kovi::tokio::sync::Mutex::const_new(());

pub async fn init() {
    let bot = plugin::get_runtime_bot();
    plugin::cron("*/10 * * * *", move || {
        let bot = std::sync::Arc::clone(&bot);
        scheduled_task(bot)
    })
    .unwrap();
}

async fn scheduled_task(bot: std::sync::Arc<RuntimeBot>) {
    let Ok(_poll_guard) = POLL_LOCK.try_lock() else {
        tracing::warn!("上一轮动态轮询尚未结束，跳过本轮");
        return;
    };
    let cfg = crate::config::read_config();
    let uids: Vec<u64> = cfg
        .dynamic_subscribe
        .iter()
        .map(|s| s.uid)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    for uid in uids {
        if let Err(e) = poll_one_uid(&bot, uid).await {
            tracing::warn!("UID={} 动态拉取失败: {e}", uid);
        }
    }
}

async fn poll_one_uid(bot: &RuntimeBot, uid: u64) -> anyhow::Result<()> {
    let page = kovi::tokio::time::timeout(Duration::from_secs(20), fetch_user_dynamics(uid, None))
        .await
        .map_err(|_| anyhow::anyhow!("fetch 超时"))??;

    let cfg = crate::config::read_config();
    let groups: Vec<i64> = cfg
        .dynamic_subscribe
        .iter()
        .filter(|s| s.uid == uid)
        .flat_map(|s| s.groups.iter().copied())
        .collect();

    if groups.is_empty() {
        return Ok(());
    }

    let items = page.items;
    let Some(latest_id) = items.iter().filter_map(dynamic_id_numeric).max() else {
        return Ok(());
    };
    let mut updates = Vec::new();

    for group in groups {
        let last_seen = cfg
            .dynamic_checkpoints
            .iter()
            .find(|checkpoint| checkpoint.uid == uid && checkpoint.group == group)
            .map(|checkpoint| checkpoint.last_seen);
        let Some(last_seen) = last_seen else {
            updates.push((group, latest_id));
            continue;
        };

        let new_items = pending_items_after(&items, last_seen);

        let mut delivered_through = last_seen;
        for (id, item) in new_items {
            let author = author_of(item);
            if let Err(e) = push_dynamic(bot, group, &author, item).await {
                tracing::warn!("推送失败 uid={} group={}: {e}", uid, group);
                break;
            }
            delivered_through = id;
        }
        if delivered_through != last_seen {
            updates.push((group, delivered_through));
        }
    }

    if !updates.is_empty() {
        config::modify_config(|cfg| {
            for &(group, last_seen) in &updates {
                let still_subscribed = cfg.dynamic_subscribe.iter().any(|subscription| {
                    subscription.uid == uid && subscription.groups.contains(&group)
                });
                if !still_subscribed {
                    continue;
                }
                if let Some(checkpoint) = cfg
                    .dynamic_checkpoints
                    .iter_mut()
                    .find(|checkpoint| checkpoint.uid == uid && checkpoint.group == group)
                {
                    checkpoint.last_seen = checkpoint.last_seen.max(last_seen);
                } else {
                    cfg.dynamic_checkpoints
                        .push(crate::config::DynamicCheckpoint {
                            uid,
                            group,
                            last_seen,
                        });
                }
            }
        })
        .await?;
    }
    Ok(())
}

fn pending_items_after(items: &[DynamicItem], last_seen: i64) -> Vec<(i64, &DynamicItem)> {
    let mut pending: Vec<(i64, &DynamicItem)> = items
        .iter()
        .filter_map(|item| dynamic_id_numeric(item).map(|id| (id, item)))
        .filter(|(id, _)| *id > last_seen)
        .collect();
    pending.sort_by_key(|(id, _)| *id);
    pending
}

fn dynamic_id_numeric(item: &DynamicItem) -> Option<i64> {
    let s = match item {
        DynamicItem::Video { id, .. }
        | DynamicItem::Draw { id, .. }
        | DynamicItem::Opus { id, .. }
        | DynamicItem::Word { id, .. }
        | DynamicItem::Other { id, .. } => id.as_str(),
        DynamicItem::Article { id, .. } | DynamicItem::Live { id, .. } => return Some(*id),
    };
    s.parse::<i64>().ok()
}

pub fn author_of(item: &DynamicItem) -> DynamicAuthor {
    match item {
        DynamicItem::Video { author, .. }
        | DynamicItem::Draw { author, .. }
        | DynamicItem::Opus { author, .. }
        | DynamicItem::Word { author, .. }
        | DynamicItem::Article { author, .. }
        | DynamicItem::Live { author, .. }
        | DynamicItem::Other { author, .. } => author.clone(),
    }
}

/// `/dynamic fetch` 单次调用最多推送的动态数。
pub const MAX_FETCH_COUNT: usize = 20;

/// 拉取指定 uid 最新的 `count` 条动态（按时间从新到旧），自动翻页。
/// 实际返回条数可能小于 `count`（API 翻页终止或总数不足）。
pub async fn fetch_recent(uid: u64, count: usize) -> anyhow::Result<Vec<DynamicItem>> {
    let mut all: Vec<DynamicItem> = Vec::with_capacity(count);
    let mut offset: Option<String> = None;

    while all.len() < count {
        let page = kovi::tokio::time::timeout(
            Duration::from_secs(20),
            fetch_user_dynamics(uid, offset.as_deref()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("fetch 超时"))??;

        if page.items.is_empty() {
            break;
        }

        let remaining = count - all.len();
        let take = remaining.min(page.items.len());
        all.extend(page.items.into_iter().take(take));

        if !page.has_more {
            break;
        }
        match page.next_offset {
            Some(o) if !o.is_empty() => offset = Some(o),
            _ => break,
        }
    }

    Ok(all)
}

pub async fn add_subscribe(uid: u64, group: i64) -> anyhow::Result<bool> {
    let mut changed = false;
    config::modify_config(|cfg| {
        if let Some(s) = cfg.dynamic_subscribe.iter_mut().find(|s| s.uid == uid) {
            if !s.groups.contains(&group) {
                s.groups.push(group);
                changed = true;
            }
        } else {
            cfg.dynamic_subscribe.push(crate::config::DynamicSubscribe {
                uid,
                groups: vec![group],
            });
            changed = true;
        }
    })
    .await?;
    Ok(changed)
}

pub async fn remove_subscribe(uid: u64, group: i64) -> anyhow::Result<()> {
    config::modify_config(|cfg| {
        if let Some(idx) = cfg.dynamic_subscribe.iter().position(|s| s.uid == uid) {
            let s = &mut cfg.dynamic_subscribe[idx];
            s.groups.retain(|g| *g != group);
            if s.groups.is_empty() {
                cfg.dynamic_subscribe.remove(idx);
            }
        }
        cfg.dynamic_checkpoints
            .retain(|checkpoint| checkpoint.uid != uid || checkpoint.group != group);
    })
    .await
}

pub async fn list_subscribes(group: i64) -> Vec<(u64, String)> {
    let cfg = config::read_config();
    cfg.dynamic_subscribe
        .iter()
        .filter(|s| s.groups.contains(&group))
        .map(|s| (s.uid, String::new()))
        .collect()
}

pub async fn push_dynamic(
    bot: &RuntimeBot,
    group: i64,
    author: &DynamicAuthor,
    item: &DynamicItem,
) -> anyhow::Result<()> {
    let total = count_pics_total(item);
    let sent = collect_pics(item);
    let mut body = format_body(author, item);
    if total > sent.len() {
        body = format!("{}\n（还有 {} 张图片未显示）", body, total - sent.len());
    }

    let mut images: Vec<Bytes> = Vec::new();
    for src in &sent {
        match fetch_image(src).await {
            Ok(bytes) => images.push(bytes),
            Err(e) => tracing::warn!("拉取动态图片失败 {}: {e}", src),
        }
    }

    // 图片全部失败时降级为纯文本推送：确定性失败（如封面 URL 持续 403）
    // 不应让 cron 每轮都卡在同一条动态上，宁可丢图也要推进 checkpoint。
    if !sent.is_empty() && images.is_empty() {
        body.push_str("\n（图片下载失败）");
    }

    let mut msg = Message::new().add_text(body);
    for bytes in &images {
        let b64 = STANDARD.encode(bytes);
        msg.push_image(&format!("base64://{}", b64));
    }

    // 用 send_group_msg_return 真正等待 onebot 确认送达；
    // 这样调用方（/dynamic fetch、cron 推送）才能正确反映"已推送 N/N"。
    bot.send_group_msg_return(group, msg)
        .await
        .map_err(|e| anyhow::anyhow!("发送群消息失败: {e}"))?;
    Ok(())
}

/// 返回 `DynamicItem` 含有的全部图片数量（不去重、不截断）。
pub fn count_pics_total(item: &DynamicItem) -> usize {
    match item {
        DynamicItem::Video { .. } | DynamicItem::Live { .. } => 1,
        DynamicItem::Draw { pics, .. } => pics.len(),
        DynamicItem::Opus { pics, .. } => pics.len(),
        DynamicItem::Word { pics, .. } => pics.len(),
        DynamicItem::Article { covers, .. } => covers.len(),
        DynamicItem::Other { .. } => 0,
    }
}

/// 构造推送文本。
/// 投稿视频专用格式：`<name> 投稿了视频：<title>` + 可选 summary + URL，封面作为图片附件由 `push_dynamic` 发送；
/// 其他类型沿用 `header + summary + url` 的旧格式。
///
/// 健壮性：
/// - `author.name` 为空时回退到 `author.pub_action`；
/// - `author.name` 与 `author.pub_action` 同时为空时直接省略 header（避免裸冒号 / leading blank line）；
/// - Video 标题为空时回退到 summary 文本（避免出现 " 投稿了视频：\nurl" 这类裸冒号）；
/// - Video 描述（summary.text）始终作为附加段保留，不再被吞掉。
pub fn format_body(author: &DynamicAuthor, item: &DynamicItem) -> String {
    if let DynamicItem::Video { title, summary, .. } = item {
        let summary_text = summary.as_ref().map(|s| s.text.as_str()).unwrap_or("");
        let header = format_author_header(author);
        let url = push_url(item);
        return match (title.is_empty(), summary_text.is_empty()) {
            (false, false) => format!(
                "{} 投稿了视频：{}\n{}\n{}",
                header, title, summary_text, url
            ),
            (false, true) => format!("{} 投稿了视频：{}\n{}", header, title, url),
            (true, false) => format!("{}\n{}\n{}", header, summary_text, url),
            (true, true) => url,
        };
    }
    let header = format_author_header(author);
    let summary = format_summary(item);
    let url = push_url(item);
    match (header.is_empty(), summary.is_empty()) {
        (true, true) => url,
        (true, false) => format!("{}\n{}", summary, url),
        (false, true) => format!("{}\n{}", header, url),
        (false, false) => format!("{}\n{}\n{}", header, summary, url),
    }
}

/// 渲染 header = "<name> <pub_action>"，name/pub_action 各自为空时优雅退化。
fn format_author_header(author: &DynamicAuthor) -> String {
    match (author.name.is_empty(), author.pub_action.is_empty()) {
        (true, true) => String::new(),
        (true, false) => author.pub_action.clone(),
        (false, true) => author.name.clone(),
        (false, false) => format!("{} {}", author.name, author.pub_action),
    }
}

async fn fetch_image(url: &str) -> anyhow::Result<Bytes> {
    const MAX_DYNAMIC_IMAGE_BYTES: usize = 10 * 1024 * 1024;
    let bytes =
        crate::image::download_bili_image(url, MAX_DYNAMIC_IMAGE_BYTES, Duration::from_secs(15))
            .await?;
    Ok(Bytes::from(bytes))
}

const MAX_PICS_PER_PUSH: usize = 3;

pub fn collect_pics(item: &DynamicItem) -> Vec<String> {
    let raw: Vec<String> = match item {
        DynamicItem::Video { cover_url, .. } => vec![cover_url.clone()],
        DynamicItem::Draw { pics, .. } => pics.iter().map(|p| p.src.clone()).collect(),
        DynamicItem::Opus { pics, .. } => pics.clone(),
        DynamicItem::Word { pics, .. } => pics.iter().map(|p| p.src.clone()).collect(),
        DynamicItem::Article { covers, .. } => covers.clone(),
        DynamicItem::Live { cover_url, .. } => vec![cover_url.clone()],
        DynamicItem::Other { .. } => vec![],
    };
    raw.into_iter().take(MAX_PICS_PER_PUSH).collect()
}

pub fn push_url(item: &DynamicItem) -> String {
    match item {
        DynamicItem::Video { bvid, .. } => format!("https://www.bilibili.com/video/{}", bvid),
        DynamicItem::Opus { jump_url, id, .. } => {
            if !jump_url.is_empty() {
                if jump_url.starts_with("//") {
                    format!("https:{}", jump_url)
                } else {
                    jump_url.clone()
                }
            } else {
                format!("https://www.bilibili.com/opus/{}", id)
            }
        }
        DynamicItem::Draw { id, .. }
        | DynamicItem::Word { id, .. }
        | DynamicItem::Other { id, .. } => format!("https://t.bilibili.com/{}", id),
        DynamicItem::Article { id, .. } => format!("https://www.bilibili.com/read/cv{}", id),
        DynamicItem::Live { room_id, .. } => {
            format!("https://live.bilibili.com/{}", room_id)
        }
    }
}

pub fn format_summary(item: &DynamicItem) -> String {
    match item {
        DynamicItem::Video { title, summary, .. } => match summary {
            Some(s) if !s.text.is_empty() => format!("{}\n{}", title, s.text),
            _ => title.clone(),
        },
        DynamicItem::Draw { summary, pics, .. } => {
            if let Some(s) = summary
                && !s.text.is_empty()
            {
                return s.text.clone();
            }
            format!("（共 {} 张图片）", pics.len())
        }
        DynamicItem::Opus {
            title,
            summary,
            pics,
            ..
        } => {
            let text = summary.as_ref().map(|s| s.text.as_str()).unwrap_or("");
            match (title.is_empty(), text.is_empty()) {
                (false, false) => format!("{}\n{}", title, text),
                (false, true) => title.clone(),
                (true, false) => text.to_string(),
                (true, true) => format!("（共 {} 张图片）", pics.len()),
            }
        }
        DynamicItem::Word { text, .. } => text.clone(),
        DynamicItem::Article {
            title,
            summary,
            label,
            ..
        } => match (title.is_empty(), summary.text.is_empty()) {
            (false, false) => format!("{}\n{}", title, summary.text),
            (false, true) => title.clone(),
            (true, false) => summary.text.clone(),
            (true, true) => {
                if !label.is_empty() {
                    label.clone()
                } else {
                    String::new()
                }
            }
        },
        DynamicItem::Live { title, .. } => title.clone(),
        DynamicItem::Other { .. } => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kovi::tokio;

    #[tokio::test]
    async fn last_seen_populates_on_first_poll() {
        let v = DynamicItem::Video {
            id: "12345".into(),
            bvid: String::new(),
            title: String::new(),
            cover_url: String::new(),
            summary: None,
            author: DynamicAuthor::default(),
        };
        assert_eq!(dynamic_id_numeric(&v), Some(12345));
        let a = DynamicItem::Article {
            id: 678,
            title: String::new(),
            summary: crate::dynamics::types::RichText {
                text: String::new(),
            },
            covers: vec![],
            label: String::new(),
            author: DynamicAuthor::default(),
        };
        assert_eq!(dynamic_id_numeric(&a), Some(678));
    }

    #[test]
    fn pending_items_are_filtered_and_sorted_oldest_first() {
        let make_item = |id: &str| DynamicItem::Other {
            id: id.to_string(),
            author: DynamicAuthor::default(),
        };
        let items = vec![make_item("300"), make_item("100"), make_item("200")];
        let ids: Vec<i64> = pending_items_after(&items, 100)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids, vec![200, 300]);
    }
}
