use std::sync::Arc;

use kovi::PluginBuilder as plugin;
use utils::RateLimiter;
use utils::command::CommandRouter;

mod commands;
mod config;
mod fetch;
mod name;
mod scan;
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
    use crate::commands::format_bytes;

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
    fn commands_keep_short_invocations_under_one_parent() {
        let tree = command_tree();

        let ResolveOutcome::Matched(add) = tree.resolve("添加 猫") else {
            panic!("expected 添加");
        };
        assert_eq!(add.path(), ["图库", "添加"]);
        assert_eq!(add.args(), ["猫"]);

        let ResolveOutcome::Matched(nested) = tree.resolve("图库 添加 猫") else {
            panic!("expected 图库 添加");
        };
        assert_eq!(nested.path(), ["图库", "添加"]);
        assert_eq!(nested.args(), ["猫"]);

        let ResolveOutcome::Matched(draw) = tree.resolve("来只 猫") else {
            panic!("expected 来只");
        };
        assert_eq!(draw.path(), ["图库", "来只"]);
        assert_eq!(draw.args(), ["猫"]);

        let ResolveOutcome::Matched(delete) = tree.resolve("删除") else {
            panic!("expected 删除");
        };
        assert_eq!(delete.path(), ["图库", "删除"]);
        assert!(delete.args().is_empty());

        let ResolveOutcome::Matched(wipe) = tree.resolve("删除 猫") else {
            panic!("expected 删除 猫");
        };
        assert_eq!(wipe.args(), ["猫"]);

        let ResolveOutcome::Matched(alias) = tree.resolve("别名 喵 猫") else {
            panic!("expected 别名");
        };
        assert_eq!(alias.path(), ["图库", "别名"]);
        assert_eq!(alias.args(), ["喵", "猫"]);

        let ResolveOutcome::Matched(list) = tree.resolve("图库") else {
            panic!("expected 图库");
        };
        assert_eq!(list.path(), ["图库"]);
        assert!(list.args().is_empty());

        let ResolveOutcome::Matched(scan) = tree.resolve("查重 猫") else {
            panic!("expected 查重");
        };
        assert_eq!(scan.path(), ["图库", "查重"]);
        assert_eq!(scan.args(), ["猫"]);
        assert_eq!(scan.permission(), Permission::BotAdmin);

        let ResolveOutcome::Matched(next) = tree.resolve("查重 猫 下一组") else {
            panic!("expected 查重 下一组");
        };
        assert_eq!(next.args(), ["猫", "下一组"]);

        let ResolveOutcome::Matched(jump) = tree.resolve("查重 猫 3") else {
            panic!("expected 查重 3");
        };
        assert_eq!(jump.args(), ["猫", "3"]);

        let ResolveOutcome::Matched(percent) = tree.resolve("查重 猫 90%") else {
            panic!("expected 查重 with percent");
        };
        assert_eq!(percent.args(), ["猫", "90%"]);

        let ResolveOutcome::Matched(nested) = tree.resolve("图库 查重 喵") else {
            panic!("expected 图库 查重");
        };
        assert_eq!(nested.path(), ["图库", "查重"]);
        assert_eq!(nested.args(), ["喵"]);

        let ResolveOutcome::Matched(by_hash) = tree.resolve("哈希 abcdef") else {
            panic!("expected 哈希");
        };
        assert_eq!(by_hash.path(), ["图库", "哈希"]);
        assert_eq!(by_hash.args(), ["abcdef"]);
        assert_eq!(by_hash.permission(), Permission::BotAdmin);

        let ResolveOutcome::Matched(nested_hash) = tree.resolve("图库 哈希 abcdef") else {
            panic!("expected 图库 哈希");
        };
        assert_eq!(nested_hash.path(), ["图库", "哈希"]);
        assert_eq!(nested_hash.args(), ["abcdef"]);
        assert_eq!(nested_hash.permission(), Permission::BotAdmin);

        let ResolveOutcome::Matched(del_hash) = tree.resolve("删除哈希 abcdef") else {
            panic!("expected 删除哈希");
        };
        assert_eq!(del_hash.path(), ["图库", "删除哈希"]);
        assert_eq!(del_hash.args(), ["abcdef"]);
        assert_eq!(del_hash.permission(), Permission::BotAdmin);
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
