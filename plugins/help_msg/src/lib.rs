use kovi::PluginBuilder as plugin;
use kovi::tokio::sync::RwLock;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

#[derive(Clone, Debug)]
pub struct HelpItem {
    pub command: String,
    pub description: String,
    pub usage: Option<String>,
}

type HelpRegistry = Arc<RwLock<HashMap<String, HelpItem>>>;

static HELP_REGISTRY: LazyLock<HelpRegistry> =
    LazyLock::new(|| Arc::new(RwLock::new(HashMap::new())));

/// 注册帮助信息
pub async fn register_help(command: String, description: String, usage: Option<String>) {
    let item = HelpItem {
        command: command,
        description,
        usage,
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

        // 处理 /help <command> 格式
        if text.starts_with("/help ") {
            let command = text.strip_prefix("/help ").unwrap().trim();
            if let Some(help_item) = get_help(command).await {
                let mut response =
                    format!("📖 `{}` - {}\n", help_item.command, help_item.description);
                if let Some(usage) = help_item.usage {
                    response.push_str(&format!("用法: {}\n", usage));
                }
                event.reply(response);
            } else {
                event.reply(format!("❌ 命令 `{}` 的帮助信息不存在", command));
            }
            return;
        }

        // 处理 help 或 /help 显示所有命令
        match text {
            "/help" => {
                let help_items = get_all_help().await;
                if help_items.is_empty() {
                    event.reply("暂无帮助信息");
                } else {
                    let mut response = String::from("📚 可用命令:\n");
                    for item in help_items {
                        response.push_str(&format!("• `{}`: {}\n", item.command, item.description));
                    }
                    response.push_str("\n使用 `/help <命令>` 查看详细用法");
                    event.reply(response);
                }
            }
            _ => {}
        }
    });
}
