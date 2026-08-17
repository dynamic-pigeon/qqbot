use std::sync::Arc;

use kovi::PluginBuilder as plugin;
use utils::RateLimiter;
use utils::command::CommandRouter;

mod commands;
mod fetch;
mod name;
mod store;

use commands::{
    DRAW_MAX_PER_WINDOW, DRAW_WINDOW, add_command, alias_command, delete_command, draw_command,
    list_command, unalias_command,
};
use store::Store;

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    let store = Arc::new(Store::open(bot.get_data_path()).expect("初始化图库存储失败"));
    let limiter = Arc::new(RateLimiter::new(DRAW_WINDOW, DRAW_MAX_PER_WINDOW));
    CommandRouter::new("image_lib", bot)
        .register(add_command(Arc::clone(&store)))
        .register(draw_command(Arc::clone(&store), limiter))
        .register(delete_command(Arc::clone(&store)))
        .register(alias_command(Arc::clone(&store)))
        .register(unalias_command(Arc::clone(&store)))
        .register(list_command(store))
        .install()
        .expect("注册图库命令失败");
}

#[cfg(test)]
mod tests {
    use utils::command::{CommandTree, ResolveOutcome};

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
        Arc::new(Store::open(dir).unwrap())
    }

    #[test]
    fn commands_resolve_first_word_and_reject_extra_via_args() {
        let store = dummy_store();
        let tree = CommandTree::new(vec![
            add_command(Arc::clone(&store)),
            draw_command(
                Arc::clone(&store),
                Arc::new(RateLimiter::new(DRAW_WINDOW, DRAW_MAX_PER_WINDOW)),
            ),
            delete_command(Arc::clone(&store)),
            alias_command(Arc::clone(&store)),
            unalias_command(Arc::clone(&store)),
            list_command(store),
        ])
        .unwrap();

        let ResolveOutcome::Matched(add) = tree.resolve("添加 猫") else {
            panic!("expected 添加");
        };
        assert_eq!(add.path(), ["添加"]);
        assert_eq!(add.args(), ["猫"]);

        let ResolveOutcome::Matched(draw) = tree.resolve("来只 猫") else {
            panic!("expected 来只");
        };
        assert_eq!(draw.args(), ["猫"]);

        let ResolveOutcome::Matched(delete) = tree.resolve("删除") else {
            panic!("expected 删除");
        };
        assert!(delete.args().is_empty());

        let ResolveOutcome::Matched(wipe) = tree.resolve("删除 猫") else {
            panic!("expected 删除 猫");
        };
        assert_eq!(wipe.args(), ["猫"]);

        let ResolveOutcome::Matched(alias) = tree.resolve("别名 喵 猫") else {
            panic!("expected 别名");
        };
        assert_eq!(alias.args(), ["喵", "猫"]);
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
    }
}
