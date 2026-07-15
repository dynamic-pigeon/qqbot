# Unified Command Router Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Build the `utils::command` command tree and migrate every first-party bot command to its registration, permission, error, and help system.

**Architecture:** Each plugin owns a `CommandRouter` that installs one Kovi `MsgEvent` listener inside that plugin's lifecycle. A pure `CommandTree` resolves text into an owned invocation; a thin router checks scope and bot-admin permission, invokes an async handler, and renders safe errors. A process-wide catalog stores metadata only so `help_msg` can render help without a reverse dependency from business plugins.

**Tech Stack:** Rust 2024, Kovi 0.13, kovi-onebot 0.13, Tokio primitives re-exported by Kovi, Cargo workspace tests.

---

## File Map

- Create `plugins/utils/src/command/mod.rs`: public re-exports for the command API.
- Create `plugins/utils/src/command/model.rs`: command nodes, handlers, arguments, permissions, scopes, and error types.
- Create `plugins/utils/src/command/tree.rs`: registration validation, Unicode token spans, longest-path resolution, and access checks.
- Create `plugins/utils/src/command/catalog.rs`: metadata snapshots and global help lookup.
- Create `plugins/utils/src/command/router.rs`: Kovi event adapter, command context, async dispatch, logging, and replies.
- Create `plugins/utils/tests/command.rs`: public API behavior tests for the pure core and catalog store.
- Modify `plugins/utils/src/lib.rs`: export `command`.
- Modify `plugins/utils/Cargo.toml`: add `kovi-onebot`.
- Modify `plugins/help_msg/src/lib.rs` and `plugins/help_msg/Cargo.toml`: replace the manual registry with `/help` backed by the catalog.
- Modify `plugins/markdown/src/lib.rs` and `plugins/markdown/Cargo.toml`: register and handle `!md` through the router.
- Modify `plugins/yu_gi_oh/src/lib.rs` and `plugins/yu_gi_oh/Cargo.toml`: register and handle `/查卡` through the router.
- Modify `plugins/msg_rank/src/lib.rs`, `plugins/msg_rank/src/msg_rank/mod.rs`, `plugins/msg_rank/src/word_cloud.rs`, and `plugins/msg_rank/Cargo.toml`: register `#今日发言排行` and the `/wordcloud` tree through one plugin router.
- Modify `plugins/bilibili/src/lib.rs` and `plugins/bilibili/Cargo.toml`: register the `/live` and `/dynamic` trees.
- Modify `Cargo.lock`: record workspace dependency graph changes if Cargo rewrites it.

### Task 1: Pure Command Tree and Argument Model

**Files:**
- Create: `plugins/utils/src/command/mod.rs`
- Create: `plugins/utils/src/command/model.rs`
- Create: `plugins/utils/src/command/tree.rs`
- Create: `plugins/utils/tests/command.rs`
- Modify: `plugins/utils/src/lib.rs`
- Modify: `plugins/utils/Cargo.toml`

- [x] **Step 1: Write failing public API tests for exact roots, deep paths, aliases, Unicode whitespace, and raw rest**

Add tests that build executable nodes with a no-op async handler and assert the desired owned resolution:

```rust
use utils::command::{Command, CommandTree, ResolveOutcome};

fn endpoint(name: &str) -> Command {
    Command::new(name).handler(|_| async { Ok(()) })
}

#[test]
fn resolves_longest_command_path_and_preserves_rest() {
    let tree = CommandTree::new(vec![
        Command::new("/admin").subcommand(
            Command::new("plugin").subcommand(
                Command::new("cache").subcommand(endpoint("clear")),
            ),
        ),
    ])
    .unwrap();

    let ResolveOutcome::Matched(command) =
        tree.resolve("  /admin\u{3000}plugin cache clear   value ")
    else {
        panic!("expected a command match");
    };

    assert_eq!(command.path(), ["/admin", "plugin", "cache", "clear"]);
    assert_eq!(command.args(), ["value"]);
    assert_eq!(command.rest(), "  value ");
    assert_eq!(command.trimmed_rest(), "value");
}

#[test]
fn requires_an_exact_root_and_accepts_aliases() {
    let tree = CommandTree::new(vec![Command::new("/live").subcommand(
        endpoint("rm").alias("remove"),
    )])
    .unwrap();

    assert!(matches!(tree.resolve("/livefoo"), ResolveOutcome::Ignored));
    let ResolveOutcome::Matched(command) = tree.resolve("/live remove 42") else {
        panic!("expected alias to match");
    };
    assert_eq!(command.path(), ["/live", "rm"]);
    assert_eq!(command.args(), ["42"]);
}
```

- [x] **Step 2: Run the focused test and verify RED**

Run: `cargo test -p utils --test command --locked`

Expected: compilation fails because `utils::command` and its types do not exist.

- [x] **Step 3: Implement the minimal public model and Unicode span tokenizer**

Define the final public signatures, not test-only shims:

```rust
pub type CommandResult = Result<(), CommandError>;
pub type CommandFuture = Pin<Box<dyn Future<Output = CommandResult> + Send>>;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum Permission {
    #[default]
    Everyone,
    BotAdmin,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MessageScope {
    #[default]
    Any,
    Group,
    Private,
}

impl Command {
    pub fn new(name: impl Into<String>) -> Self;
    pub fn alias(self, alias: impl Into<String>) -> Self;
    pub fn description(self, description: impl Into<String>) -> Self;
    pub fn usage(self, usage: impl Into<String>) -> Self;
    pub fn permission(self, permission: Permission) -> Self;
    pub fn scope(self, scope: MessageScope) -> Self;
    pub fn subcommand(self, child: Command) -> Self;
    pub fn handler<F, Fut>(self, handler: F) -> Self
    where
        F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CommandResult> + Send + 'static;
}
```

`CommandTree::new` recursively computes inherited permission and scope, validates siblings, and stores immutable nodes. `CommandTree::resolve` scans `char_indices`, records each non-whitespace byte range, matches canonical names or aliases, and clones only the final handler and invocation strings into `ResolvedCommand`.

- [x] **Step 4: Run the focused test and verify GREEN**

Run: `cargo test -p utils --test command --locked`

Expected: the exact-root, alias, deep-path, Unicode whitespace, arguments, and rest tests pass.

- [x] **Step 5: Add failing registration and resolution-error tests**

Add separate tests for duplicate sibling names, name/alias collisions, empty leaves, permission relaxation, missing subcommands, and unknown subcommands:

```rust
#[test]
fn rejects_permission_relaxation() {
    let result = CommandTree::new(vec![
        Command::new("/admin")
            .permission(Permission::BotAdmin)
            .subcommand(
                endpoint("status").permission(Permission::Everyone),
            ),
    ]);
    assert!(matches!(
        result,
        Err(CommandRegistrationError::PermissionRelaxation { .. })
    ));
}

#[test]
fn reports_unknown_subcommand_at_the_deepest_node() {
    let tree = CommandTree::new(vec![
        Command::new("/live")
            .usage("/live <add|list>")
            .subcommand(endpoint("add"))
            .subcommand(endpoint("list")),
    ])
    .unwrap();
    let ResolveOutcome::Error(error) = tree.resolve("/live nope") else {
        panic!("expected a route error");
    };
    assert_eq!(error.to_string(), "未知子命令 `nope`\n用法: /live <add|list>");
}
```

- [x] **Step 6: Verify RED, implement validation and route errors, then verify GREEN**

Run before implementation: `cargo test -p utils --test command --locked`

Expected before implementation: the new tests fail on missing validation or the wrong route result.

Implement `CommandRegistrationError`, `RouteError`, usage rendering, and recursive validation. Run the same command again and expect all command tests to pass.

- [x] **Step 7: Format and commit the pure core**

Run: `cargo fmt --all`

Run: `cargo test -p utils --test command --locked`

Commit:

```bash
git add plugins/utils/Cargo.toml plugins/utils/src/lib.rs plugins/utils/src/command plugins/utils/tests/command.rs Cargo.lock
git commit -m "feat(utils): add command tree core"
```

### Task 2: Catalog, Access Checks, and Kovi Router

**Files:**
- Create: `plugins/utils/src/command/catalog.rs`
- Create: `plugins/utils/src/command/router.rs`
- Modify: `plugins/utils/src/command/mod.rs`
- Modify: `plugins/utils/src/command/model.rs`
- Modify: `plugins/utils/src/command/tree.rs`
- Modify: `plugins/utils/tests/command.rs`

- [x] **Step 1: Write failing tests for access checks, safe errors, and catalog replacement**

Add tests with these exact expectations:

```rust
#[test]
fn checks_scope_and_permission_before_dispatch() {
    assert_eq!(
        check_access(MessageScope::Group, Permission::Everyone, MessageSource::Private, false),
        Err(AccessError::GroupOnly)
    );
    assert_eq!(
        check_access(MessageScope::Group, Permission::BotAdmin, MessageSource::Group, false),
        Err(AccessError::PermissionDenied)
    );
    assert_eq!(
        check_access(MessageScope::Group, Permission::BotAdmin, MessageSource::Group, true),
        Ok(())
    );
}

#[test]
fn catalog_replaces_an_owners_root_and_resolves_help_paths() {
    let mut catalog = CatalogStore::default();
    catalog.register("first", live_tree()).unwrap();
    catalog.register("first", updated_live_tree()).unwrap();

    let help = catalog.find(&["live", "remove"]).unwrap();
    assert_eq!(help.command.path, ["/live", "rm"]);
    assert_eq!(catalog.roots().len(), 1);
}

#[test]
fn internal_errors_are_not_exposed() {
    let error = CommandError::internal(anyhow::anyhow!("database password"));
    assert_eq!(render_command_error(&error, "/test"), "命令执行失败，请稍后重试");
}
```

- [x] **Step 2: Run the focused test and verify RED**

Run: `cargo test -p utils --test command --locked`

Expected: compilation fails because access, catalog, and error-rendering APIs are missing.

- [x] **Step 3: Implement the catalog and pure access/error functions**

Implement these public entry points:

```rust
pub fn check_access(
    scope: MessageScope,
    permission: Permission,
    source: MessageSource,
    is_admin: bool,
) -> Result<(), AccessError>;

impl CommandCatalog {
    pub fn roots() -> Vec<CommandMetadata>;
    pub fn find(path: &[&str]) -> Option<CommandHelp>;
}

pub fn render_command_error(error: &CommandError, usage: &str) -> String;
```

Use a `LazyLock<RwLock<CatalogStore>>` for the process-wide catalog. `CatalogStore::register` rejects roots already owned by a different plugin and atomically replaces all entries for the same `owner + root`.

- [x] **Step 4: Run the focused test and verify GREEN**

Run: `cargo test -p utils --test command --locked`

Expected: all pure access, catalog, and safe-rendering tests pass.

- [x] **Step 5: Write a failing async dispatch-order test**

Factor dispatch gating so it accepts an invocation callback independent of Kovi, then assert the callback is not called after scope or permission rejection:

```rust
#[tokio::test]
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
            Ok(())
        },
    )
    .await;

    assert_eq!(result, Err(AccessError::PermissionDenied));
    assert!(!called.load(Ordering::SeqCst));
}
```

Also build a real `kovi::Message` with multiple text segments and assert that the adapter's text extraction preserves leading and trailing whitespace around Markdown content.

- [x] **Step 6: Verify RED, implement `CommandRouter::install`, then verify GREEN**

Run before implementation: `cargo test -p utils --test command --locked`

Expected before implementation: compilation fails on `dispatch_if_allowed`.

Implement `CommandContext`, `CommandArguments`, and `CommandRouter`. `install()` must validate the tree and catalog before calling `PluginBuilder::on_msg`; the listener must rebuild untrimmed command text from `MsgEvent::message`, resolve the command tree, check scope and permission before both route-error and handler replies, query `RuntimeBot::get_all_admin()`, build the context, await the cloned handler, log internal errors with command/user/group fields, and reply through `RepliableEvent`.

Run after implementation: `cargo test -p utils --test command --locked`

Expected: all tests pass, including proof that rejected access does not invoke a callback.

- [x] **Step 7: Format, check the utils crate, and commit**

Run:

```bash
cargo fmt --all
cargo clippy -p utils --all-targets --locked -- -D warnings
cargo test -p utils --all-targets --locked
```

Commit:

```bash
git add plugins/utils/src/command plugins/utils/tests/command.rs plugins/utils/Cargo.toml Cargo.lock
git commit -m "feat(utils): add command routing and catalog"
```

### Task 3: Catalog-Backed Help Command

**Files:**
- Modify: `plugins/help_msg/Cargo.toml`
- Modify: `plugins/help_msg/src/lib.rs`
- Modify: `plugins/utils/tests/command.rs`

- [x] **Step 1: Write failing catalog help-rendering tests**

Add tests for the exact root list, parent help, leaf help, leading slash normalization, alias lookup, and missing paths:

```rust
#[test]
fn renders_root_parent_and_leaf_help() {
    let catalog = populated_catalog();
    assert!(catalog.render_help(&[]).contains("`/live`: 管理直播订阅"));
    assert!(catalog.render_help(&["live"]).contains("`add`: 添加直播订阅"));
    assert!(catalog.render_help(&["/live", "add"]).contains("用法: /live add <uid>"));
    assert!(catalog.render_help(&["live", "remove"]).contains("别名: remove"));
    assert_eq!(catalog.render_help(&["missing"]), "命令 `missing` 的帮助信息不存在");
}
```

- [x] **Step 2: Verify RED, implement help rendering, and verify GREEN**

Run before implementation: `cargo test -p utils --test command --locked`

Expected before implementation: tests fail because `render_help` is missing.

Implement deterministic `BTreeMap` ordering and render only command metadata. Run the focused test again and expect it to pass.

- [x] **Step 3: Replace `help_msg`'s manual registry with a routed `/help` handler**

The plugin should register one root command and pass whitespace arguments to the catalog:

```rust
#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    CommandRouter::new("help_msg", bot)
        .register(
            Command::new("/help")
                .description("查看可用命令和具体用法")
                .usage("/help [命令路径]")
                .handler(|ctx| async move {
                    let path = ctx.args().iter().map(String::as_str).collect::<Vec<_>>();
                    ctx.reply(CommandCatalog::render_help(&path));
                    Ok(())
                }),
        )
        .install()
        .expect("注册 /help 命令失败");
}
```

Delete `HelpItem`, `HELP_REGISTRY`, `register_help`, `get_all_help`, and `get_help`. Add the `utils` workspace dependency.

- [x] **Step 4: Format, test, and commit**

Run:

```bash
cargo fmt --all
cargo test -p utils --test command --locked
cargo check -p help_msg --locked
```

Commit:

```bash
git add plugins/help_msg plugins/utils/tests/command.rs Cargo.lock
git commit -m "refactor(help): generate help from command catalog"
```

### Task 4: Markdown and Card Query Commands

**Files:**
- Modify: `plugins/markdown/src/lib.rs`
- Modify: `plugins/markdown/Cargo.toml`
- Modify: `plugins/yu_gi_oh/src/lib.rs`
- Modify: `plugins/yu_gi_oh/Cargo.toml`

- [x] **Step 1: Add failing command-definition tests inside both plugin crates**

Extract `markdown_command()` and `card_query_command()` builders. Test their public resolution through `CommandTree`: `!md` without content resolves the registered root, `!md   # title` preserves two leading spaces in `rest`, `/查卡` resolves with no arguments, and `/查卡 青眼 白龙` produces `trimmed_rest() == "青眼 白龙"`.

Run: `cargo test -p markdown -p yu_gi_oh --lib --locked`

Expected: compilation fails because the builder functions and router registrations do not exist.

- [x] **Step 2: Migrate `!md` with unified input and internal errors**

Register `!md` with description `根据 Markdown 生成图片` and usage `!md <Markdown 内容>`. The handler must return `MissingArgument { name: "Markdown 内容" }` for empty `rest()`, preserve the existing 32 KiB limit and cooldown as `CommandError::User`, return rendering failures as `CommandError::Internal`, and send the generated image with `reply_and_quote`.

- [x] **Step 3: Migrate `/查卡` with trimmed raw input**

Register `/查卡` with description `查询游戏王卡片信息` and usage `/查卡 <卡片名称>`. The handler must use `trimmed_rest()`, return a missing argument for empty input, preserve the 128-byte limit and cooldown, convert fetch failures to internal errors, and preserve the text fallback when only image fetching fails.

- [x] **Step 4: Remove direct `help_msg` dependencies and verify GREEN**

Run:

```bash
cargo fmt --all
cargo test -p markdown -p yu_gi_oh --lib --locked
cargo check -p markdown -p yu_gi_oh --locked
```

Expected: definition tests and both crate checks pass.

- [x] **Step 5: Commit both simple command migrations**

```bash
git add plugins/markdown plugins/yu_gi_oh Cargo.lock
git commit -m "refactor(commands): migrate markdown and card query"
```

### Task 5: Message Rank and Word Cloud Command Trees

**Files:**
- Modify: `plugins/msg_rank/src/lib.rs`
- Modify: `plugins/msg_rank/src/msg_rank/mod.rs`
- Modify: `plugins/msg_rank/src/word_cloud.rs`
- Modify: `plugins/msg_rank/Cargo.toml`

- [x] **Step 1: Write failing command-tree tests in `word_cloud.rs`**

Extract a `wordcloud_command(path: Arc<PathBuf>) -> Command` builder and test that `once`, `enable`, `disable`, and `status` resolve under `/wordcloud`, inherit `MessageScope::Group` and `Permission::BotAdmin`, while `/wordcloud` returns the parent usage.

Run: `cargo test -p msg_rank word_cloud::tests::command_tree --lib --locked`

Expected: compilation fails because `wordcloud_command` does not exist.

- [x] **Step 2: Write a failing daily-rank command test**

Extract `daily_rank_command() -> Command` and test that `#今日发言排行` resolves as `MessageScope::Group` plus `Permission::Everyone`, while `#今日发言排行榜` is ignored as a different root.

Run: `cargo test -p msg_rank msg_rank::command_tests::daily_rank_is_a_public_group_command --lib --locked`

Expected: compilation fails because `daily_rank_command` does not exist.

- [x] **Step 3: Replace `cmd_handler` with four async routed handlers**

The `once` closure captures `Arc<PathBuf>`, replies with the progress message, then spawns `send_word_cloud`. The other three handlers use `ctx.event().group_id`, call `modify_config` or `read_config`, and map configuration failures to `CommandError::Internal`. Register the root before cron jobs and delete the repeated `get_all_admin` check and string match.

- [x] **Step 4: Route the daily rank handler and install one plugin router**

Move the existing cooldown, rendering, screenshot, and image reply into a `CommandContext` handler. Install `daily_rank_command()` and `wordcloud_command()` together from `plugins/msg_rank/src/lib.rs`, preserving the message collection listener and word-cloud cron jobs.

- [x] **Step 5: Verify GREEN and commit**

Run:

```bash
cargo fmt --all
cargo test -p msg_rank word_cloud::tests::command_tree --lib --locked
cargo check -p msg_rank --locked
```

Commit:

```bash
git add plugins/msg_rank Cargo.lock
git commit -m "refactor(msg-rank): route word cloud commands"
```

### Task 6: Bilibili Live and Dynamic Command Trees

**Files:**
- Modify: `plugins/bilibili/src/lib.rs`
- Modify: `plugins/bilibili/Cargo.toml`

- [x] **Step 1: Write failing tree-definition tests**

Extract `live_command()` and `dynamic_command()` builders. Test that list is `Everyone`, mutating/fetch nodes are `BotAdmin`, every node is `Group`, `remove` resolves to canonical `rm`, `/livefoo` is ignored, and a three-level synthetic command remains covered by the shared utils tests.

Run: `cargo test -p bilibili command_tests --lib --locked`

Expected: compilation fails because the builders do not exist.

- [x] **Step 2: Migrate `/live` handlers**

Split `exec_cmd` into `live_add`, `live_remove`, and `live_list` handlers taking `CommandContext`. Parse `uid` with `parse_arg::<u64>(0, "uid")`, reject extra arguments, use `ctx.event().group_id.expect("群命令已通过范围校验")`, and preserve existing subscription behavior and user-safe API/configuration errors.

- [x] **Step 3: Migrate `/dynamic` handlers**

Split `dynamic_cmd` into `dynamic_add`, `dynamic_remove`, `dynamic_list`, and `dynamic_fetch`. Parse and bound `count` exactly as `1..=MAX_FETCH_COUNT`, reject extra arguments, preserve push-result replies, and map unexpected failures to the documented error class.

- [x] **Step 4: Install one router and preserve non-command listeners**

Register both roots in `CommandRouter::new("bilibili", bot)`, install it once, retain `plugin::on_group_msg(parse_bv)`, `living::init()`, and `dynamics::init()`, and delete `exec_cmd`, `dynamic_cmd`, and `is_admin`.

- [x] **Step 5: Verify GREEN and commit**

Run:

```bash
cargo fmt --all
cargo test -p bilibili command_tests --lib --locked
cargo test -p bilibili --all-targets --locked
cargo check -p bilibili --locked
```

Commit:

```bash
git add plugins/bilibili Cargo.lock
git commit -m "refactor(bilibili): route subscription commands"
```

### Task 7: Dependency and Behavior Audit

**Files:**
- Modify: `plugins/markdown/Cargo.toml`
- Modify: `plugins/yu_gi_oh/Cargo.toml`
- Modify: `plugins/msg_rank/Cargo.toml`
- Modify: `plugins/bilibili/Cargo.toml`
- Modify: `Cargo.lock`

- [x] **Step 1: Prove duplicate help registration is gone**

Run:

```bash
rg -n "help_msg::register_help|help_msg\.workspace" plugins
```

Expected: no output outside historical documentation. Remove any remaining business-plugin dependency or call before continuing.

- [x] **Step 2: Prove ad hoc command matching is gone for migrated roots**

Run:

```bash
rg -n 'starts_with\("/(live|dynamic|wordcloud)|strip_prefix\("/(查卡|wordcloud)|strip_prefix\("!md|split_whitespace' plugins --glob '*.rs'
```

Expected: no output from the migrated command handlers. URL parsing and unrelated text parsing remain unchanged.

- [x] **Step 3: Run workspace formatting and compilation**

Run:

```bash
cargo fmt --all
cargo check --workspace --all-targets --locked
git diff --check
```

Expected: every command exits successfully with no warnings or whitespace errors.

- [x] **Step 4: Commit cleanup if the previous tasks did not already leave a clean tree**

```bash
git add Cargo.toml Cargo.lock plugins
git commit -m "chore(commands): finish command router migration"
```

Skip this commit only when `git status --short` is already empty.

### Task 8: Full Verification and Documentation Check

**Files:**
- Verify: `docs/superpowers/specs/2026-07-15-command-router-design.md`
- Verify: all workspace sources and tests

- [x] **Step 1: Run the complete required checks from a clean build state**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Expected: all three commands exit with status 0; ignored network tests may remain reported as ignored.

- [x] **Step 2: Audit every design requirement against source and tests**

Use `rg` and the test names to confirm arbitrary-depth matching, aliases, raw rest, two permissions, inherited scope, safe internal errors, catalog-backed help, all six first-party roots, and untouched `.kovi` mounting. Any missing evidence requires a new failing test before a fix.

- [x] **Step 3: Inspect the final branch and commit verification-only changes**

Run:

```bash
git status --short --branch
git log --oneline --decorate master..HEAD
git diff --stat master...HEAD
```

Expected: the branch contains the design commit and focused implementation commits, with no uncommitted source changes. If formatting or test corrections changed files, commit them with `git commit -m "test(commands): complete router verification"` after staging only those files.
