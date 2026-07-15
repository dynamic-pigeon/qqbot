use std::{
    fmt::Write,
    sync::{Arc, LazyLock},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::{
    Message, PluginBuilder as plugin, RuntimeBot,
    serde_json::{self, Value},
};
use kovi_onebot::{EventRegistrar as _, MessageRegistrar as _, event::GroupMsgEvent};

use crate::{
    bv_parser::parse_url,
    living::{check_uid, fetch_uid_names},
};

mod bv_parser;
mod config;
pub mod dynamics;
mod image;
mod living;

static USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 Edg/147.0.0.0";

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::REFERER,
        reqwest::header::HeaderValue::from_static("https://www.bilibili.com/"),
    );
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(10))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(16)
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
});

#[kovi::plugin]
async fn main() {
    config::init().await.unwrap();
    // 启动期强制解析硬编码 SPACE_FEED_URL，让 URL 被未来编辑损坏时立刻 panic，
    // 而不是等到第一次 /dynamic fetch / cron 才暴露。
    dynamics::warm_up();
    help_msg::register_help(
        "直播订阅",
        "管理本群的 B 站直播订阅",
        "/live add <uid> - 为本群订阅指定 uid 的开播通知（管理员专用）\n/live rm <uid> - 取消本群的订阅（管理员专用）\n/live list - 查看本群订阅列表",
    )
    .await;
    let bot = plugin::get_runtime_bot();
    let bot_for_exec = Arc::clone(&bot);
    let bot_for_dyn = Arc::clone(&bot);
    plugin::on_group_msg(move |event| {
        let bot = Arc::clone(&bot_for_exec);
        exec_cmd(event, bot)
    });
    plugin::on_group_msg(parse_bv);
    plugin::on_group_msg(move |event| {
        let bot = Arc::clone(&bot_for_dyn);
        dynamic_cmd(event, bot)
    });
    living::init().await;
    dynamics::init().await;
}

async fn exec_cmd(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    let text = event.borrow_text().unwrap_or_default();
    let text = text.trim();

    // support: /live add <uid>
    //          /live rm <uid>
    //          /live status
    if !text.starts_with("/live") {
        return;
    }

    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 2 {
        event.reply("用法: /live add <uid> | /live rm <uid> | /live list");
        return;
    }

    match parts[1] {
        "add" => {
            if !bot
                .get_all_admin()
                .unwrap_or_default()
                .iter()
                .any(|id| id.try_as_i64() == Some(event.user_id))
            {
                event.reply("❌ 管理员专用命令，普通用户无法使用");
                return;
            }
            if parts.len() < 3 {
                event.reply("请指定要订阅的 uid，例如: /live add 672328094");
                return;
            }
            let uid = match parts[2].parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    event.reply("uid 格式错误，需为整数");
                    return;
                }
            };

            if !check_uid(uid).await {
                event.reply("该 uid 没有直播间或者 bot 网络错误");
                return;
            }

            let group = event.group_id;
            match config::modify_config(|cfg| {
                if let Some(sub) = cfg.subscribe.iter_mut().find(|s| s.uid == uid) {
                    if !sub.groups.contains(&group) {
                        sub.groups.push(group);
                    }
                } else {
                    cfg.subscribe.push(crate::config::Subscribe {
                        uid,
                        groups: vec![group],
                    });
                }
            })
            .await
            {
                Ok(_) => event.reply(format!("已为本群订阅 uid={}", uid)),
                Err(e) => event.reply(format!("订阅失败: {}", e)),
            }
        }
        "rm" | "remove" => {
            if !bot
                .get_all_admin()
                .unwrap_or_default()
                .iter()
                .any(|id| id.try_as_i64() == Some(event.user_id))
            {
                event.reply("❌ 管理员专用命令，普通用户无法使用");
                return;
            }
            if parts.len() < 3 {
                event.reply("请指定要取消订阅的 uid，例如: /live rm 672328094");
                return;
            }
            let uid = match parts[2].parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    event.reply("uid 格式错误，需为整数");
                    return;
                }
            };
            let group = event.group_id;
            match config::modify_config(|cfg| {
                if let Some(idx) = cfg.subscribe.iter().position(|s| s.uid == uid) {
                    let sub = &mut cfg.subscribe[idx];
                    sub.groups.retain(|g| *g != group);
                    if sub.groups.is_empty() {
                        cfg.subscribe.remove(idx);
                    }
                }
            })
            .await
            {
                Ok(_) => event.reply(format!("已取消本群对 uid={} 的订阅", uid)),
                Err(e) => event.reply(format!("取消订阅失败: {}", e)),
            }
        }
        "list" => {
            let group = event.group_id;
            let cfg = config::read_config().clone();
            let uids: Vec<u64> = cfg
                .subscribe
                .iter()
                .filter(|s| s.groups.contains(&group))
                .map(|s| s.uid)
                .collect();
            if uids.is_empty() {
                event.reply("本群尚未订阅任何直播间");
            } else {
                let names = match fetch_uid_names(&uids).await {
                    Ok(names) => names,
                    Err(e) => {
                        event.reply(format!("查询订阅列表失败: {}", e));
                        return;
                    }
                };

                let mut text = String::from("订阅列表：");
                for (uid, name) in names {
                    write!(&mut text, "\n{} ({})", name, uid).unwrap();
                }
                event.reply(text);
            }
        }
        _ => {
            event.reply("未知子命令，支持: add | rm | list");
        }
    }
}

async fn dynamic_cmd(event: Arc<GroupMsgEvent>, bot: Arc<RuntimeBot>) {
    let text = event.borrow_text().unwrap_or_default();
    let text = text.trim();
    if !text.starts_with("/dynamic") {
        return;
    }
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 2 {
        event.reply(
            "用法: /dynamic add <uid> | /dynamic rm <uid> | /dynamic list | /dynamic fetch <uid>",
        );
        return;
    }
    let group = event.group_id;

    match parts[1] {
        "add" => {
            if !is_admin(&bot, event.user_id) {
                event.reply("❌ 管理员专用命令");
                return;
            }
            if parts.len() < 3 {
                event.reply("请指定 uid，例如: /dynamic add 672328094");
                return;
            }
            let uid = match parts[2].parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    event.reply("uid 格式错误");
                    return;
                }
            };
            match dynamics::add_subscribe(uid, group).await {
                Ok(true) => event.reply(format!("已为本群订阅动态 uid={}", uid)),
                Ok(false) => event.reply(format!("本群已订阅 uid={}", uid)),
                Err(e) => event.reply(format!("订阅失败: {e}")),
            }
        }
        "rm" => {
            if !is_admin(&bot, event.user_id) {
                event.reply("❌ 管理员专用命令");
                return;
            }
            if parts.len() < 3 {
                event.reply("请指定 uid，例如: /dynamic rm 672328094");
                return;
            }
            let uid = match parts[2].parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    event.reply("uid 格式错误");
                    return;
                }
            };
            match dynamics::remove_subscribe(uid, group).await {
                Ok(_) => event.reply(format!("已取消本群对 uid={} 的动态订阅", uid)),
                Err(e) => event.reply(format!("取消失败: {e}")),
            }
        }
        "list" => {
            let entries = dynamics::list_subscribes(group).await;
            if entries.is_empty() {
                event.reply("本群尚未订阅任何动态");
                return;
            }
            let uids: Vec<u64> = entries.iter().map(|(u, _)| *u).collect();
            let names = match crate::living::fetch_uid_names(&uids).await {
                Ok(m) => m,
                Err(e) => {
                    event.reply(format!("查询 UP 名失败: {e}"));
                    return;
                }
            };
            let mut out = String::from("动态订阅列表：");
            for uid in uids {
                let name = names.get(&uid).cloned().unwrap_or_default();
                out.push_str(&format!("\n{} ({})", name, uid));
            }
            event.reply(out);
        }
        "fetch" => {
            if !is_admin(&bot, event.user_id) {
                event.reply("❌ 管理员专用命令");
                return;
            }
            if parts.len() < 3 {
                event.reply(format!(
                    "用法: /dynamic fetch <uid> [count]，count 默认 1，最大 {}",
                    dynamics::MAX_FETCH_COUNT
                ));
                return;
            }
            let uid = match parts[2].parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    event.reply("uid 格式错误");
                    return;
                }
            };
            let count: usize = if parts.len() >= 4 {
                match parts[3].parse::<usize>() {
                    Ok(n) if (1..=dynamics::MAX_FETCH_COUNT).contains(&n) => n,
                    _ => {
                        event.reply(format!("count 必须是 1..={}", dynamics::MAX_FETCH_COUNT));
                        return;
                    }
                }
            } else {
                1
            };
            match dynamics::fetch_recent(uid, count).await {
                Ok(items) if items.is_empty() => {
                    event.reply(format!("uid={} 无动态", uid));
                }
                Ok(items) => {
                    let mut pushed = 0usize;
                    for item in &items {
                        let author = dynamics::author_of(item);
                        if let Err(e) = dynamics::push_dynamic(&bot, group, &author, item).await {
                            tracing::warn!("渲染失败 uid={}: {e}", uid);
                        } else {
                            pushed += 1;
                        }
                    }
                    event.reply(format!("已推送 {}/{} 条动态", pushed, items.len()));
                }
                Err(e) => {
                    event.reply(format!("拉取失败: {e}"));
                }
            }
        }
        _ => {
            event.reply("未知子命令，支持: add | rm | list | fetch");
        }
    }
}

fn is_admin(bot: &Arc<RuntimeBot>, user_id: i64) -> bool {
    bot.get_all_admin()
        .unwrap_or_default()
        .iter()
        .any(|id| id.try_as_i64() == Some(user_id))
}

async fn parse_bv(event: Arc<GroupMsgEvent>) {
    for msg in event.message.iter() {
        let bv_info = match msg.kind.as_str() {
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

                parse_url(url, event.group_id).await
            }
            "text" => {
                let Some(text) = msg.data.get("text").and_then(|v| v.as_str()) else {
                    continue;
                };
                parse_url(text, event.group_id).await
            }
            _ => continue,
        };

        let bv_info = match bv_info {
            Ok(info) => info,
            Err(e) => {
                if !matches!(e, bv_parser::BvError::ParseFailed(_)) {
                    tracing::error!("解析 BV 失败: {}", e);
                }
                continue;
            }
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
