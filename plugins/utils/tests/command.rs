use kovi::Message;
use kovi_onebot::MessageRegistrar as _;
use utils::command::{
    AccessError, CatalogStore, Command, CommandArguments, CommandError, CommandRegistrationError,
    CommandTree, MessageScope, MessageSource, Permission, ResolveOutcome, RouteError, check_access,
    extract_command_text, render_command_error,
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
fn inherits_scope_and_permission_and_allows_tightening() {
    let inherited = CommandTree::new(vec![
        Command::new("/admin")
            .scope(MessageScope::Group)
            .permission(Permission::BotAdmin)
            .subcommand(endpoint("status")),
    ])
    .unwrap();
    let ResolveOutcome::Matched(status) = inherited.resolve("/admin status") else {
        panic!("expected inherited command to resolve");
    };
    assert_eq!(status.scope(), MessageScope::Group);
    assert_eq!(status.permission(), Permission::BotAdmin);

    let tightened = CommandTree::new(vec![
        Command::new("/tools").subcommand(endpoint("reload").permission(Permission::BotAdmin)),
    ])
    .unwrap();
    let ResolveOutcome::Matched(reload) = tightened.resolve("/tools reload") else {
        panic!("expected tightened command to resolve");
    };
    assert_eq!(reload.permission(), Permission::BotAdmin);
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
fn route_errors_retain_the_deepest_nodes_access_rules() {
    let tree = CommandTree::new(vec![
        Command::new("/admin")
            .scope(MessageScope::Group)
            .permission(Permission::BotAdmin)
            .usage("/admin <status>")
            .subcommand(endpoint("status")),
    ])
    .unwrap();
    let ResolveOutcome::Error(error) = tree.resolve("/admin nope") else {
        panic!("expected a route error");
    };

    assert_eq!(error.scope(), MessageScope::Group);
    assert_eq!(error.permission(), Permission::BotAdmin);
}

#[test]
fn checks_scope_and_permission() {
    assert_eq!(
        check_access(
            MessageScope::Any,
            Permission::Everyone,
            MessageSource::Group,
            false,
        ),
        Ok(())
    );
    assert_eq!(
        check_access(
            MessageScope::Any,
            Permission::Everyone,
            MessageSource::Private,
            false,
        ),
        Ok(())
    );
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
            MessageScope::Private,
            Permission::Everyone,
            MessageSource::Group,
            false,
        ),
        Err(AccessError::PrivateOnly)
    );
    assert_eq!(
        check_access(
            MessageScope::Private,
            Permission::Everyone,
            MessageSource::Private,
            false,
        ),
        Ok(())
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
fn exposed_subcommands_resolve_without_parent_prefix() {
    let tree = CommandTree::new(vec![
        Command::new("图库")
            .handler(|_| async { Ok(()) })
            .subcommand(endpoint("添加").expose_as_root())
            .subcommand(endpoint("来只")),
    ])
    .unwrap();

    let ResolveOutcome::Matched(add) = tree.resolve("添加 猫") else {
        panic!("expected exposed subcommand to resolve");
    };
    assert_eq!(path(&add), ["图库", "添加"]);
    assert_eq!(add.args(), ["猫"]);

    let ResolveOutcome::Matched(nested) = tree.resolve("图库 添加 猫") else {
        panic!("expected nested path to resolve");
    };
    assert_eq!(path(&nested), ["图库", "添加"]);

    assert!(matches!(tree.resolve("来只 猫"), ResolveOutcome::Ignored));

    let ResolveOutcome::Matched(parent) = tree.resolve("图库") else {
        panic!("expected parent handler to resolve");
    };
    assert_eq!(path(&parent), ["图库"]);
}

#[test]
fn rejects_exposed_root_name_conflicts_with_real_roots() {
    let conflict = CommandTree::new(vec![
        endpoint("添加"),
        Command::new("图库")
            .handler(|_| async { Ok(()) })
            .subcommand(endpoint("添加").expose_as_root()),
    ]);
    assert!(matches!(
        conflict,
        Err(CommandRegistrationError::DuplicateName { .. })
    ));
}

#[test]
fn catalog_groups_exposed_subcommands_under_parent_root() {
    let tree = CommandTree::new(vec![
        Command::new("图库")
            .description("管理本群图库")
            .handler(|_| async { Ok(()) })
            .subcommand(
                endpoint("添加")
                    .description("写入图库")
                    .usage("添加 <库名>")
                    .expose_as_root(),
            ),
    ])
    .unwrap();
    let mut catalog = CatalogStore::default();
    catalog.register("image_lib", &tree).unwrap();

    assert_eq!(catalog.roots().len(), 1);
    assert!(catalog.render_help(&[]).contains("`图库`: 管理本群图库"));
    assert!(!catalog.render_help(&[]).contains("`添加`"));
    assert!(catalog.render_help(&["图库"]).contains("`添加`: 写入图库"));

    let help = catalog.find(&["添加"]).unwrap();
    assert_eq!(help.command.path, ["图库", "添加"]);
    assert!(catalog.render_help(&["添加"]).contains("用法: 添加 <库名>"));
}

#[test]
fn catalog_rejects_exposed_root_conflicts_between_plugins() {
    let grouped = CommandTree::new(vec![
        Command::new("图库")
            .handler(|_| async { Ok(()) })
            .subcommand(endpoint("添加").expose_as_root()),
    ])
    .unwrap();
    let other = CommandTree::new(vec![endpoint("添加")]).unwrap();
    let mut catalog = CatalogStore::default();
    catalog.register("image_lib", &grouped).unwrap();

    assert!(matches!(
        catalog.register("other", &other),
        Err(CommandRegistrationError::RootConflict { ref root, .. }) if root == "添加"
    ));
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

#[test]
fn help_lookup_accepts_a_hash_command_without_its_prefix() {
    let tree = CommandTree::new(vec![endpoint("#今日发言排行")]).unwrap();
    let mut catalog = CatalogStore::default();
    catalog.register("msg_rank", &tree).unwrap();

    let help = catalog.find(&["今日发言排行"]).unwrap();
    assert_eq!(help.command.path, ["#今日发言排行"]);
}

#[test]
fn extracts_message_text_without_trimming_raw_content() {
    let message = Message::new()
        .add_text("  !md   first  ")
        .add_image("base64://ignored")
        .add_text("  second  ");

    assert_eq!(
        extract_command_text(&message).as_deref(),
        Some("  !md   first  \n  second  ")
    );
    assert_eq!(extract_command_text(&Message::new()), None);
}
