use std::sync::Arc;

use kovi::{Message, PluginBuilder as plugin, futures_util::future::join_all};
use kovi_onebot::{EventRegistrar as _, event::GroupMsgEvent};
use utils::command::{
    Command, CommandContext, CommandError, CommandResult, CommandRouter, MessageScope, Permission,
};

mod config;
mod db;
mod msg_rank;
pub mod ocr;
mod word_cloud;

const MAX_OCR_IMAGES_PER_MESSAGE: usize = 3;
const MAX_STORED_MESSAGE_BYTES: usize = 4 * 1024;

#[kovi::plugin]
async fn main() {
    let _ = config::static_config();
    ocr::preload_config();

    let bot = plugin::get_runtime_bot();
    let path = Arc::new(bot.get_data_path());

    let config_path = path.join("config.json");
    if let Err(e) = config::CONFIG.init(config_path) {
        tracing::error!("初始化配置失败: {e}");
        return;
    }

    let db_path = path.join("msg.db");
    if !db_path.exists() {
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        std::fs::File::create(&db_path).unwrap();
    }

    db::init_db(&db_path).await.unwrap();
    plugin::drop(|| async move { db::flush_on_shutdown().await });

    CommandRouter::new("msg_rank", Arc::clone(&bot))
        .register(record_command())
        .register(word_cloud::wordcloud_command(Arc::clone(&path)))
        .register(msg_rank::rank_command())
        .register(msg_rank::weekly_report::weekly_report_command(Arc::clone(
            &path,
        )))
        .install()
        .expect("注册发言排行、消息采集、词云与周报命令失败");

    plugin::on_group_msg(add_msg);
    word_cloud::init(Arc::clone(&bot), Arc::clone(&path)).unwrap();
    msg_rank::weekly_report::init(Arc::clone(&bot), Arc::clone(&path)).unwrap();
}

fn record_command() -> Command {
    Command::new("/消息采集")
        .description("启用或停用本群消息采集")
        .usage("/消息采集 <enable|disable|status>")
        .scope(MessageScope::Group)
        .permission(Permission::BotAdmin)
        .subcommand(
            Command::new("enable")
                .description("启用本群消息采集")
                .usage("/消息采集 enable")
                .sync_handler(record_enable),
        )
        .subcommand(
            Command::new("disable")
                .description("停用本群消息采集")
                .usage("/消息采集 disable")
                .sync_handler(record_disable),
        )
        .subcommand(
            Command::new("status")
                .description("查看本群消息采集状态")
                .usage("/消息采集 status")
                .sync_handler(record_status),
        )
}

fn record_enable(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    config::CONFIG
        .modify(|config| config.enable_recording(group_id))
        .map_err(CommandError::internal)?;
    ctx.reply("消息采集已启用");
    Ok(())
}

fn record_disable(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    config::CONFIG
        .modify(|config| config.disable_recording(group_id))
        .map_err(CommandError::internal)?;
    ctx.reply("消息采集已停用");
    Ok(())
}

fn record_status(ctx: CommandContext) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    ctx.reply(if config::CONFIG.get().recording_enabled(group_id) {
        "消息采集已启用"
    } else {
        "消息采集未启用"
    });
    Ok(())
}

async fn add_msg(event: Arc<GroupMsgEvent>) {
    let group = event.group_id;
    let user = event.user_id;

    if !config::CONFIG.get().recording_enabled(group) {
        return;
    }

    let text = truncate_utf8(get_text(&event.message).await, MAX_STORED_MESSAGE_BYTES);
    if text.trim().is_empty() {
        return;
    }

    if let Err(e) = db::add_msg(group, user, text, event.time) {
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
                if let Some(url) = utils::https_image_url_from_data(&seg.data) {
                    image_count += 1;
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
    use utils::command::{Permission, ResolveOutcome};

    #[test]
    fn truncate_utf8_never_splits_a_character() {
        assert_eq!(truncate_utf8("ab中文".to_string(), 5), "ab中");
        assert_eq!(truncate_utf8("short".to_string(), 10), "short");
    }

    #[test]
    fn record_commands_are_admin_group_commands() {
        let tree = utils::command::CommandTree::new(vec![super::record_command()]).unwrap();
        for name in ["enable", "disable", "status"] {
            let ResolveOutcome::Matched(command) = tree.resolve(&format!("/消息采集 {name}"))
            else {
                panic!("expected /消息采集 {name} to resolve");
            };
            assert_eq!(command.permission(), Permission::BotAdmin);
            assert_eq!(command.scope(), utils::command::MessageScope::Group);
        }
    }
}
