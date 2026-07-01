use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{Arc, LazyLock},
};

use kovi::{Message, PluginBuilder as plugin, tokio::sync::RwLock};
use kovi_onebot::*;

#[derive(Clone, Debug)]
pub struct HelpItem {
    pub command: String,
    pub description: String,
    pub usage: Message,
}

type HelpRegistry = Arc<RwLock<BTreeMap<String, HelpItem>>>;

static HELP_REGISTRY: LazyLock<HelpRegistry> =
    LazyLock::new(|| Arc::new(RwLock::new(BTreeMap::new())));

/// 注册帮助信息
pub async fn register_help(
    command: impl Into<String>,
    description: impl Into<String>,
    usage: impl Into<Message>,
) {
    let item = HelpItem {
        command: command.into(),
        description: description.into(),
        usage: usage.into(),
    };
    let mut registry = HELP_REGISTRY.write().await;
    registry.insert(item.command.clone(), item);
}

/// 获取所有帮助信息
async fn get_all_help() -> Vec<HelpItem> {
    let registry = HELP_REGISTRY.read().await;
    registry.values().cloned().collect()
}

/// 获取特定命令的帮助信息
async fn get_help(command: &str) -> Option<HelpItem> {
    let registry = HELP_REGISTRY.read().await;
    registry.get(command).cloned()
}

#[kovi::plugin]
async fn main() {
    plugin::on_msg(|event| async move {
        let text = event.borrow_text().unwrap_or_default();

        if text.starts_with("/help ") {
            let command = text.strip_prefix("/help ").unwrap().trim();
            if command.is_empty() {
                event.reply("⚠️ 请提供命令名，例如 `/help md`");
                return;
            }
            if let Some(help_item) = get_help(command).await {
                event.reply(help_item.usage);
            } else {
                event.reply(format!("❌ 命令 `{}` 的帮助信息不存在", command));
            }
            return;
        }

        if text == "/help" {
            let help_items = get_all_help().await;
            if help_items.is_empty() {
                event.reply("暂无帮助信息");
            } else {
                let mut response = String::from("📚 可用命令:\n");
                for item in help_items {
                    let _ = writeln!(response, "• `{}`: {}", item.command, item.description);
                }
                response.push_str("\n使用 `/help <命令>` 查看详细用法");
                event.reply(response);
            }
        }
    });
}
