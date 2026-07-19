use std::{
    fmt::Write,
    sync::{Arc, LazyLock},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use kovi::{
    Message, PluginBuilder as plugin,
    serde_json::{self, Value},
};
use kovi_onebot::{EventRegistrar as _, MessageRegistrar as _, event::GroupMsgEvent};
use utils::command::{
    Command, CommandContext, CommandError, CommandResult, CommandRouter, MessageScope, Permission,
};

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
    let bot = plugin::get_runtime_bot();
    CommandRouter::new("bilibili", Arc::clone(&bot))
        .register(live_command())
        .register(dynamic_command())
        .install()
        .expect("注册 Bilibili 命令失败");

    plugin::on_group_msg(parse_bv);
    living::init().await;
    dynamics::init().await;
}

fn live_command() -> Command {
    Command::new("/live")
        .description("管理本群的 B 站直播订阅")
        .usage("/live <add|rm|list>")
        .scope(MessageScope::Group)
        .subcommand(
            Command::new("add")
                .description("添加直播订阅")
                .usage("/live add <uid>")
                .permission(Permission::BotAdmin)
                .handler(live_add),
        )
        .subcommand(
            Command::new("rm")
                .alias("remove")
                .description("移除直播订阅")
                .usage("/live rm <uid>")
                .permission(Permission::BotAdmin)
                .handler(live_remove),
        )
        .subcommand(
            Command::new("list")
                .description("查看本群直播订阅")
                .usage("/live list")
                .handler(live_list),
        )
}

fn dynamic_command() -> Command {
    Command::new("/dynamic")
        .description("管理本群的 B 站动态订阅")
        .usage("/dynamic <add|rm|list|fetch>")
        .scope(MessageScope::Group)
        .subcommand(
            Command::new("add")
                .description("添加动态订阅")
                .usage("/dynamic add <uid>")
                .permission(Permission::BotAdmin)
                .handler(dynamic_add),
        )
        .subcommand(
            Command::new("rm")
                .description("移除动态订阅")
                .usage("/dynamic rm <uid>")
                .permission(Permission::BotAdmin)
                .handler(dynamic_remove),
        )
        .subcommand(
            Command::new("list")
                .description("查看本群动态订阅")
                .usage("/dynamic list")
                .handler(dynamic_list),
        )
        .subcommand(
            Command::new("fetch")
                .description("立即拉取并推送最近动态")
                .usage("/dynamic fetch <uid> [count]")
                .permission(Permission::BotAdmin)
                .handler(dynamic_fetch),
        )
}

// 群命令按 MessageScope::Group 声明，group_id 理论上必有值；
// 缺失时按内部错误上报而不是 panic，避免框架行为变化时击垮消息循环。
fn group_id(ctx: &CommandContext) -> Result<i64, CommandError> {
    ctx.event()
        .group_id
        .ok_or_else(|| CommandError::internal(anyhow::anyhow!("群命令事件缺少 group_id")))
}

async fn live_add(ctx: CommandContext) -> CommandResult {
    let uid = ctx.parse_arg::<u64>(0, "uid")?;
    ctx.ensure_no_extra_args(1)?;
    if !check_uid(uid).await {
        return Err(CommandError::user("该 uid 没有直播间或者 bot 网络错误"));
    }

    let group = group_id(&ctx)?;
    config::modify_config(|config| {
        if let Some(subscription) = config.subscribe.iter_mut().find(|item| item.uid == uid) {
            if !subscription.groups.contains(&group) {
                subscription.groups.push(group);
            }
        } else {
            config.subscribe.push(crate::config::Subscribe {
                uid,
                groups: vec![group],
            });
        }
    })
    .await
    .map_err(CommandError::internal)?;
    ctx.reply(format!("已为本群订阅 uid={uid}"));
    Ok(())
}

async fn live_remove(ctx: CommandContext) -> CommandResult {
    let uid = ctx.parse_arg::<u64>(0, "uid")?;
    ctx.ensure_no_extra_args(1)?;
    let group = group_id(&ctx)?;
    config::modify_config(|config| {
        if let Some(index) = config.subscribe.iter().position(|item| item.uid == uid) {
            let subscription = &mut config.subscribe[index];
            subscription.groups.retain(|item| *item != group);
            if subscription.groups.is_empty() {
                config.subscribe.remove(index);
            }
        }
    })
    .await
    .map_err(CommandError::internal)?;
    ctx.reply(format!("已取消本群对 uid={uid} 的订阅"));
    Ok(())
}

async fn live_list(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group = group_id(&ctx)?;
    let uids = config::read_config()
        .subscribe
        .iter()
        .filter(|subscription| subscription.groups.contains(&group))
        .map(|subscription| subscription.uid)
        .collect::<Vec<_>>();
    if uids.is_empty() {
        ctx.reply("本群尚未订阅任何直播间");
        return Ok(());
    }

    let names = fetch_uid_names(&uids)
        .await
        .map_err(CommandError::internal)?;
    let mut output = String::from("订阅列表：");
    for (uid, name) in names {
        write!(&mut output, "\n{name} ({uid})").unwrap();
    }
    ctx.reply(output);
    Ok(())
}

async fn dynamic_add(ctx: CommandContext) -> CommandResult {
    let uid = ctx.parse_arg::<u64>(0, "uid")?;
    ctx.ensure_no_extra_args(1)?;
    let group = group_id(&ctx)?;
    let added = dynamics::add_subscribe(uid, group)
        .await
        .map_err(CommandError::internal)?;
    ctx.reply(if added {
        format!("已为本群订阅动态 uid={uid}")
    } else {
        format!("本群已订阅 uid={uid}")
    });
    Ok(())
}

async fn dynamic_remove(ctx: CommandContext) -> CommandResult {
    let uid = ctx.parse_arg::<u64>(0, "uid")?;
    ctx.ensure_no_extra_args(1)?;
    let group = group_id(&ctx)?;
    dynamics::remove_subscribe(uid, group)
        .await
        .map_err(CommandError::internal)?;
    ctx.reply(format!("已取消本群对 uid={uid} 的动态订阅"));
    Ok(())
}

async fn dynamic_list(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group = group_id(&ctx)?;
    let entries = dynamics::list_subscribes(group).await;
    if entries.is_empty() {
        ctx.reply("本群尚未订阅任何动态");
        return Ok(());
    }

    let uids = entries.iter().map(|(uid, _)| *uid).collect::<Vec<_>>();
    let names = fetch_uid_names(&uids)
        .await
        .map_err(CommandError::internal)?;
    let mut output = String::from("动态订阅列表：");
    for uid in uids {
        let name = names.get(&uid).cloned().unwrap_or_default();
        write!(&mut output, "\n{name} ({uid})").unwrap();
    }
    ctx.reply(output);
    Ok(())
}

fn parse_dynamic_fetch_count(value: Option<&str>) -> Result<usize, CommandError> {
    let count = match value {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| CommandError::InvalidArgument {
                name: "count".to_owned(),
            })?,
        None => 1,
    };
    if !(1..=dynamics::MAX_FETCH_COUNT).contains(&count) {
        return Err(CommandError::user(format!(
            "count 必须是 1..={}",
            dynamics::MAX_FETCH_COUNT
        )));
    }
    Ok(count)
}

async fn dynamic_fetch(ctx: CommandContext) -> CommandResult {
    let uid = ctx.parse_arg::<u64>(0, "uid")?;
    let count = parse_dynamic_fetch_count(ctx.arg(1))?;
    ctx.ensure_no_extra_args(2)?;

    let group = group_id(&ctx)?;
    let items = dynamics::fetch_recent(uid, count)
        .await
        .map_err(CommandError::internal)?;
    if items.is_empty() {
        ctx.reply(format!("uid={uid} 无动态"));
        return Ok(());
    }

    let mut pushed = 0;
    for item in &items {
        let author = dynamics::author_of(item);
        if let Err(error) = dynamics::push_dynamic(ctx.bot(), group, &author, item).await {
            tracing::warn!("渲染失败 uid={uid}: {error}");
        } else {
            pushed += 1;
        }
    }
    ctx.reply(format!("已推送 {pushed}/{} 条动态", items.len()));
    Ok(())
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

#[cfg(test)]
mod command_tests {
    use utils::command::{CommandError, MessageScope, Permission, ResolveOutcome};

    fn resolved(tree: &utils::command::CommandTree, input: &str) -> (Permission, MessageScope) {
        let ResolveOutcome::Matched(command) = tree.resolve(input) else {
            panic!("expected {input} to resolve");
        };
        (command.permission(), command.scope())
    }

    #[test]
    fn live_tree_has_group_scope_alias_and_mixed_permissions() {
        let tree = utils::command::CommandTree::new(vec![super::live_command()]).unwrap();

        assert_eq!(
            resolved(&tree, "/live list"),
            (Permission::Everyone, MessageScope::Group)
        );
        for command in ["/live add 1", "/live rm 1", "/live remove 1"] {
            assert_eq!(
                resolved(&tree, command),
                (Permission::BotAdmin, MessageScope::Group)
            );
        }
        let ResolveOutcome::Matched(alias) = tree.resolve("/live remove 1") else {
            panic!("expected remove alias to resolve");
        };
        assert_eq!(alias.path(), ["/live", "rm"]);
        assert!(matches!(tree.resolve("/livefoo"), ResolveOutcome::Ignored));
    }

    #[test]
    fn dynamic_tree_has_public_list_and_admin_mutations() {
        let tree = utils::command::CommandTree::new(vec![super::dynamic_command()]).unwrap();

        assert_eq!(
            resolved(&tree, "/dynamic list"),
            (Permission::Everyone, MessageScope::Group)
        );
        for command in ["/dynamic add 1", "/dynamic rm 1", "/dynamic fetch 1 2"] {
            assert_eq!(
                resolved(&tree, command),
                (Permission::BotAdmin, MessageScope::Group)
            );
        }
    }

    #[test]
    fn dynamic_fetch_count_defaults_and_enforces_bounds() {
        assert_eq!(super::parse_dynamic_fetch_count(None).unwrap(), 1);
        assert_eq!(
            super::parse_dynamic_fetch_count(Some(&super::dynamics::MAX_FETCH_COUNT.to_string()))
                .unwrap(),
            super::dynamics::MAX_FETCH_COUNT
        );
        for value in ["0", "999999"] {
            assert!(matches!(
                super::parse_dynamic_fetch_count(Some(value)),
                Err(CommandError::User(_))
            ));
        }
        assert!(matches!(
            super::parse_dynamic_fetch_count(Some("not-a-number")),
            Err(CommandError::InvalidArgument { ref name }) if name == "count"
        ));
    }
}
