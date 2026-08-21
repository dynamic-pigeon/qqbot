use std::sync::Arc;

use kovi::PluginBuilder as plugin;
use utils::RateLimiter;
use utils::command::CommandRouter;

mod commands;
mod config;
mod fetch;
mod name;
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
    use utils::command::{CatalogStore, CommandTree, ResolveOutcome};

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
    }

    #[test]
    fn help_lists_only_the_parent_root() {
        let tree = command_tree();
        let mut catalog = CatalogStore::default();
        catalog.register("image_lib", &tree).unwrap();

        assert_eq!(catalog.roots().len(), 1);
        assert_eq!(catalog.roots()[0].path, ["图库"]);
        let roots = catalog.render_help(&[]);
        assert!(roots.contains("`图库`: 管理本群图库"));
        assert!(!roots.contains("`添加`"));

        let parent = catalog.render_help(&["图库"]);
        assert!(parent.contains("`添加`: 回复一张或多张图，写入本群指定图库"));
        assert!(parent.contains("`来只`:"));
        assert!(parent.contains("`删除`:"));
        assert!(parent.contains("`别名`:"));
        assert!(parent.contains("`取消别名`:"));

        let add = catalog.find(&["添加"]).unwrap();
        assert_eq!(add.command.path, ["图库", "添加"]);
        assert!(catalog.render_help(&["添加"]).contains("用法: 添加 <库名>"));
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
