use std::sync::Arc;

use kovi::PluginBuilder as plugin;
use utils::RateLimiter;
use utils::command::CommandRouter;

mod commands;
mod config;
mod fetch;
mod name;
mod scan;
mod send;
mod similar;
mod store;

use commands::image_lib_command;
use store::Store;

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    let store = Arc::new(Store::open(bot.get_data_path()).expect("初始化图库存储失败"));
    let image_config = config::static_config();
    let limiter = Arc::new(RateLimiter::new(
        image_config.draw_window(),
        image_config.draw_max_per_window(),
    ));
    CommandRouter::new("image_lib", bot)
        .register(image_lib_command(store, limiter))
        .install()
        .expect("注册图库命令失败");
}

#[cfg(test)]
mod tests {
    use utils::command::{CommandTree, Permission, ResolveOutcome};

    use super::*;

    fn dummy_store() -> Arc<Store> {
        let dir = std::env::temp_dir().join(format!(
            "image_lib_cmd_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Arc::new(Store::open_with_quota(dir, u64::MAX).unwrap())
    }

    fn command_tree() -> CommandTree {
        let store = dummy_store();
        let config = crate::config::StaticConfig::default();
        CommandTree::new(vec![image_lib_command(
            store,
            Arc::new(RateLimiter::new(
                config.draw_window(),
                config.draw_max_per_window(),
            )),
        )])
        .unwrap()
    }

    #[test]
    fn root_aliases_share_parent_and_admin_keeps_permission() {
        let tree = command_tree();

        let ResolveOutcome::Matched(add) = tree.resolve("添加 猫") else {
            panic!("expected 添加");
        };
        assert_eq!(add.path(), ["图库", "添加"]);
        assert_eq!(add.args(), ["猫"]);

        let ResolveOutcome::Matched(nested) = tree.resolve("图库 添加 猫") else {
            panic!("expected 图库 添加");
        };
        assert_eq!(nested.path(), add.path());
        assert_eq!(nested.args(), add.args());

        let ResolveOutcome::Matched(list) = tree.resolve("图库") else {
            panic!("expected 图库");
        };
        assert_eq!(list.path(), ["图库"]);

        let ResolveOutcome::Matched(scan) = tree.resolve("查重 猫") else {
            panic!("expected 查重");
        };
        assert_eq!(scan.path(), ["图库", "查重"]);
        assert_eq!(scan.permission(), Permission::BotAdmin);
    }
}
