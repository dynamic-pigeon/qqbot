use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use utils::command::{
    AccessError, CatalogStore, Command, CommandArguments, CommandError, CommandRegistrationError,
    CommandTree, MessageScope, MessageSource, Permission, ResolveOutcome, RouteError, check_access,
    dispatch_if_allowed, render_command_error,
};

fn endpoint(name: &str) -> Command {
    Command::new(name).handler(|_| async { Ok(()) })
}

fn path(command: &utils::command::ResolvedCommand) -> Vec<&str> {
    command.path().iter().map(String::as_str).collect()
}

#[test]
fn resolves_longest_command_path_and_preserves_rest() {
    let tree = CommandTree::new(vec![Command::new("/admin").subcommand(
        Command::new("plugin").subcommand(Command::new("cache").subcommand(endpoint("clear"))),
    )])
    .unwrap();

    let ResolveOutcome::Matched(command) =
        tree.resolve("  /admin\u{3000}plugin cache clear   value ")
    else {
        panic!("expected a command match");
    };

    assert_eq!(path(&command), ["/admin", "plugin", "cache", "clear"]);
    assert_eq!(command.args(), ["value"]);
    assert_eq!(command.rest(), "  value ");
    assert_eq!(command.trimmed_rest(), "value");
}

#[test]
fn requires_an_exact_root_and_accepts_aliases() {
    let tree = CommandTree::new(vec![
        Command::new("/live").subcommand(endpoint("rm").alias("remove")),
    ])
    .unwrap();

    assert!(matches!(tree.resolve("/livefoo"), ResolveOutcome::Ignored));

    let ResolveOutcome::Matched(command) = tree.resolve("/live remove 42") else {
        panic!("expected alias to match");
    };
    assert_eq!(path(&command), ["/live", "rm"]);
    assert_eq!(command.args(), ["42"]);
}

#[test]
fn rejects_duplicate_roots_and_sibling_alias_conflicts() {
    let duplicate_roots = CommandTree::new(vec![endpoint("/help"), endpoint("/help")]);
    assert!(matches!(
        duplicate_roots,
        Err(CommandRegistrationError::DuplicateName { .. })
    ));

    let alias_conflict = CommandTree::new(vec![
        Command::new("/live")
            .subcommand(endpoint("add").alias("list"))
            .subcommand(endpoint("list")),
    ]);
    assert!(matches!(
        alias_conflict,
        Err(CommandRegistrationError::DuplicateName { .. })
    ));
}

#[test]
fn rejects_empty_leaf_and_permission_relaxation() {
    let empty_leaf = CommandTree::new(vec![Command::new("/empty")]);
    assert!(matches!(
        empty_leaf,
        Err(CommandRegistrationError::EmptyLeaf { .. })
    ));

    let relaxation = CommandTree::new(vec![
        Command::new("/admin")
            .permission(Permission::BotAdmin)
            .subcommand(endpoint("status").permission(Permission::Everyone)),
    ]);
    assert!(matches!(
        relaxation,
        Err(CommandRegistrationError::PermissionRelaxation { .. })
    ));
}

#[test]
fn reports_missing_and_unknown_subcommands_at_the_deepest_node() {
    let tree = CommandTree::new(vec![
        Command::new("/live")
            .usage("/live <add|list>")
            .subcommand(endpoint("add"))
            .subcommand(endpoint("list")),
    ])
    .unwrap();

    let ResolveOutcome::Error(RouteError::MissingSubcommand { path, .. }) = tree.resolve("/live")
    else {
        panic!("expected a missing-subcommand error");
    };
    assert_eq!(path, ["/live"]);

    let ResolveOutcome::Error(error) = tree.resolve("/live nope") else {
        panic!("expected an unknown-subcommand error");
    };
    assert_eq!(
        error.to_string(),
        "未知子命令 `nope`\n用法: /live <add|list>\n可用子命令: add | list"
    );
}

#[test]
fn checks_scope_and_permission() {
    assert_eq!(
        check_access(
            MessageScope::Group,
            Permission::Everyone,
            MessageSource::Private,
            false,
        ),
        Err(AccessError::GroupOnly)
    );
    assert_eq!(
        check_access(
            MessageScope::Group,
            Permission::BotAdmin,
            MessageSource::Group,
            false,
        ),
        Err(AccessError::PermissionDenied)
    );
    assert_eq!(
        check_access(
            MessageScope::Group,
            Permission::BotAdmin,
            MessageSource::Group,
            true,
        ),
        Ok(())
    );
}

#[test]
fn parses_arguments_and_renders_safe_errors() {
    let arguments = CommandArguments::new(vec!["42".to_owned()], "42".to_owned());
    assert_eq!(arguments.parse_arg::<u64>(0, "uid").unwrap(), 42);
    assert!(matches!(
        arguments.parse_arg::<u64>(1, "count"),
        Err(CommandError::MissingArgument { ref name }) if name == "count"
    ));
    assert!(matches!(
        CommandArguments::new(vec!["x".to_owned()], "x".to_owned())
            .parse_arg::<u64>(0, "uid"),
        Err(CommandError::InvalidArgument { ref name }) if name == "uid"
    ));
    assert!(matches!(
        arguments.ensure_no_extra_args(0),
        Err(CommandError::UnexpectedArgument)
    ));

    assert_eq!(
        render_command_error(
            &CommandError::MissingArgument {
                name: "uid".to_owned(),
            },
            "/live add <uid>",
        ),
        "缺少参数 `uid`\n用法: /live add <uid>"
    );
    assert_eq!(
        render_command_error(
            &CommandError::internal(anyhow::anyhow!("database password")),
            "/test",
        ),
        "命令执行失败，请稍后重试"
    );
}

fn live_tree(description: &str) -> CommandTree {
    CommandTree::new(vec![
        Command::new("/live")
            .description(description)
            .usage("/live <add|rm>")
            .subcommand(
                endpoint("add")
                    .description("添加直播订阅")
                    .usage("/live add <uid>")
                    .permission(Permission::BotAdmin),
            )
            .subcommand(
                endpoint("rm")
                    .alias("remove")
                    .description("移除直播订阅")
                    .usage("/live rm <uid>")
                    .permission(Permission::BotAdmin),
            ),
    ])
    .unwrap()
}

#[test]
fn catalog_replaces_an_owners_root_and_resolves_alias_paths() {
    let mut catalog = CatalogStore::default();
    catalog
        .register("bilibili", &live_tree("管理直播订阅"))
        .unwrap();
    catalog
        .register("bilibili", &live_tree("管理本群直播订阅"))
        .unwrap();

    let help = catalog.find(&["live", "remove"]).unwrap();
    assert_eq!(help.command.path, ["/live", "rm"]);
    assert_eq!(help.command.description, "移除直播订阅");
    assert_eq!(catalog.roots().len(), 1);
    assert_eq!(catalog.roots()[0].description, "管理本群直播订阅");
}

#[test]
fn catalog_rejects_root_alias_conflicts_between_plugins() {
    let first = CommandTree::new(vec![endpoint("/first").alias("/shared")]).unwrap();
    let second = CommandTree::new(vec![endpoint("/shared")]).unwrap();
    let mut catalog = CatalogStore::default();
    catalog.register("first", &first).unwrap();

    assert!(matches!(
        catalog.register("second", &second),
        Err(CommandRegistrationError::RootConflict { ref root, .. }) if root == "/shared"
    ));
}

#[test]
fn renders_root_parent_leaf_and_missing_help() {
    let mut catalog = CatalogStore::default();
    catalog
        .register("bilibili", &live_tree("管理直播订阅"))
        .unwrap();

    assert!(catalog.render_help(&[]).contains("`/live`: 管理直播订阅"));
    assert!(
        catalog
            .render_help(&["live"])
            .contains("`add`: 添加直播订阅")
    );
    assert!(
        catalog
            .render_help(&["/live", "add"])
            .contains("用法: /live add <uid>")
    );
    assert!(
        catalog
            .render_help(&["live", "remove"])
            .contains("别名: remove")
    );
    assert_eq!(
        catalog.render_help(&["missing"]),
        "命令 `missing` 的帮助信息不存在"
    );
}

#[kovi::tokio::test]
async fn rejected_access_does_not_invoke_the_handler() {
    let called = Arc::new(AtomicBool::new(false));
    let marker = Arc::clone(&called);

    let result = dispatch_if_allowed(
        MessageScope::Group,
        Permission::BotAdmin,
        MessageSource::Group,
        false,
        move || async move {
            marker.store(true, Ordering::SeqCst);
        },
    )
    .await;

    assert_eq!(result, Err(AccessError::PermissionDenied));
    assert!(!called.load(Ordering::SeqCst));
}
