# 统一命令路由设计

## 背景

当前自研插件分别通过 `starts_with`、`strip_prefix` 和 `split_whitespace` 识别命令。相同的命令边界、权限校验、参数错误和帮助信息因此散落在多个插件中，并产生以下问题：

- `/livefoo` 会被 `/live` 的前缀判断误认为命令。
- `/wordcloud`、`/查卡` 和 `!md` 缺少参数时的行为不一致。
- 管理员校验和通用错误回复被重复实现。
- 命令实现和 `help_msg::register_help` 分开维护，容易出现帮助信息过期。
- 多级子命令没有统一的匹配、权限继承和错误处理规则。

本设计在 `utils` 中提供统一的命令注册、解析、权限校验、错误回复和帮助元数据能力，并迁移仓库内的全部自研命令。第三方 `kovi-plugin-cmd` 提供的 `.kovi` 命令不在迁移范围内。

## 目标

- 支持任意层级的命令树和同级别名。
- 每个业务插件使用同一套注册、解析、权限和错误规则。
- 保留 Kovi 原有的插件启停、访问控制和事件上下文边界。
- 支持按 Unicode 空白分词以及读取未分词的剩余原文。
- 支持普通用户与机器人管理员两级权限，并允许子命令收紧权限。
- 从命令注册信息自动生成统一帮助目录。
- 未注册的根命令静默忽略，已注册命令的错误得到明确回复。
- 命令核心逻辑不依赖正在运行的机器人，可以独立测试。

## 非目标

- 不接管 `.kovi` 或其他第三方插件命令。
- 不实现 shell 风格引号、转义符、选项解析或自动补全。
- 不引入群主、群管理员、白名单或自定义权限闭包。
- 不将所有业务处理闭包集中到一个全局路由器。
- 不自动生成业务参数模型；处理函数通过上下文辅助方法读取参数。

## 方案选择

采用“每个插件一个 `CommandRouter`”的方案。`utils` 提供统一实现，每个插件在自己的 `main` 中创建命令树并安装监听器。

没有采用全局命令中心，因为集中分发会让处理函数脱离原插件的 Kovi 启停和访问控制边界，并使插件生命周期与上下文归属变得不清晰。没有采用宏驱动注册，因为当前命令规模不足以抵消宏实现、编译错误和调试成本。

## 架构

`utils::command` 包含以下组件：

- `CommandRouter`：拥有单个插件的命令树，完成注册校验并安装该插件的消息监听器。
- `Command`：命令节点构造器，包含名称、别名、说明、用法、消息范围、权限、子节点和可选处理函数。
- `CommandContext`：处理函数上下文，包含消息事件、运行时机器人、匹配路径、位置参数和剩余原文。
- `Permission`：权限定义，第一版包含 `Everyone` 和 `BotAdmin`。
- `MessageScope`：消息来源限制，包含 `Any`、`Group` 和 `Private`。
- `CommandError`：处理参数错误、可展示业务错误和内部错误。
- `CommandCatalog`：只保存帮助展示所需的命令元数据，不保存处理闭包，不参与消息分发。

依赖关系如下：

```text
各业务插件 ──> utils::command
help_msg   ──> utils::command::CommandCatalog
```

业务插件不再直接依赖 `help_msg`。`utils` 增加对 `kovi-onebot` 的依赖，用于安装通用 `MsgEvent` 监听器和回复消息。

## 公共接口

注册接口使用树形构造：

```rust
let router = CommandRouter::new("bilibili", bot).register(
    Command::new("/live")
        .description("管理本群的 B 站直播订阅")
        .scope(MessageScope::Group)
        .permission(Permission::Everyone)
        .subcommand(
            Command::new("list")
                .usage("/live list")
                .handler(list_live),
        )
        .subcommand(
            Command::new("add")
                .usage("/live add <uid>")
                .permission(Permission::BotAdmin)
                .handler(add_live),
        )
        .subcommand(
            Command::new("rm")
                .alias("remove")
                .usage("/live rm <uid>")
                .permission(Permission::BotAdmin)
                .handler(remove_live),
        ),
);

router.install()?;
```

处理函数返回统一结果：

```rust
async fn add_live(ctx: CommandContext) -> CommandResult {
    let uid = ctx.parse_arg::<u64>(0, "uid")?;
    ctx.ensure_no_extra_args(1)?;

    // 执行业务逻辑。
    Ok(())
}
```

`CommandContext` 提供以下读取和回复接口：

```rust
ctx.arg(index)
ctx.parse_arg::<T>(index, name)
ctx.rest()
ctx.trimmed_rest()
ctx.ensure_no_extra_args(expected)
ctx.reply(message)
ctx.reply_and_quote(message)
```

异步处理函数由线程安全的装箱闭包保存。上下文持有 `Arc<MsgEvent>` 和 `Arc<RuntimeBot>`，处理函数不借用路由器内部数据，可以安全跨越 `await`。

## 命令树与匹配规则

每个 `Command` 是一个树节点。节点可以同时拥有处理函数和子节点；没有处理函数的节点仅用于组织子命令。解析器按最长路径匹配：

```text
/admin
└── plugin
    └── cache
        ├── clear
        └── status
```

输入 `/admin plugin cache clear foo` 时，匹配路径为 `/admin plugin cache clear`，剩余位置参数为 `foo`。

匹配规则如下：

1. 无纯文本的消息直接忽略。
2. 仅移除消息开头的空白；根命令必须和第一个词完整匹配。
3. 使用 Unicode 空白识别词边界，逐层匹配节点名称或别名。
4. 选择最长的已注册路径。
5. 根命令不存在时返回 `Ignored`，不产生回复。
6. 当前节点有子节点但没有处理函数时，无法匹配的下一个词属于未知子命令。
7. 当前节点有处理函数时，无法匹配的后续词作为业务参数传入。
8. 匹配到无处理函数的中间节点并结束输入时，展示该节点的用法和直接子命令。

`rest()` 返回匹配路径之后、移除一个命令分隔空白后的原始内容，不改变后续缩进和尾部空白。`trimmed_rest()` 在此基础上移除首尾空白。`!md` 使用 `rest()`，`/查卡` 使用 `trimmed_rest()`。

Kovi 适配层直接拼接 `MsgEvent::message` 中的文本段，不使用会裁剪首尾空白的 `MsgEvent::text`，保证 `rest()` 的原文契约在实际消息处理链中成立。

不支持引号和转义。需要空格的参数必须读取剩余原文。

## 权限与消息范围

权限按命令节点继承：

- 根节点未配置权限时默认为 `Everyone`。
- 子节点未配置权限时继承父节点。
- 子节点可以从 `Everyone` 收紧为 `BotAdmin`。
- 子节点不得从 `BotAdmin` 放宽为 `Everyone`，注册校验会拒绝该命令树。

`BotAdmin` 使用 `RuntimeBot::get_all_admin()` 判断，与现有管理员命令语义保持一致。

消息范围同样按节点继承。`Group` 命令要求 `MsgEvent::group_id` 存在，`Private` 命令要求其不存在，`Any` 不限制来源。范围校验和权限校验都发生在业务处理函数之前。

## 执行流程

```text
MsgEvent
  -> 提取纯文本
  -> 解析并匹配命令树
  -> 校验消息范围
  -> 校验最终权限
  -> 构造 CommandContext
  -> 调用异步业务处理函数
  -> 渲染 CommandResult
  -> event.reply
```

路由核心返回结构化结果，Kovi 适配层只负责读取事件字段、查询管理员列表、调用处理函数和发送回复。这样解析、匹配、注册校验、权限判定和错误渲染可以不启动机器人直接测试。

## 错误模型

处理函数使用以下错误分类：

```rust
pub enum CommandError {
    MissingArgument { name: String },
    InvalidArgument { name: String },
    UnexpectedArgument,
    User(String),
    Internal(anyhow::Error),
}
```

用户输入错误会自动附加最终命令节点的 `usage`。内部错误记录命令路径、用户 ID、群 ID 和完整错误链，只向用户展示固定文本，不泄露内部信息。

| 场景 | 行为 |
| --- | --- |
| 未注册根命令 | 静默忽略 |
| 未知子命令 | 提示未知子命令，并展示当前节点用法 |
| 中间节点缺少子命令 | 展示当前节点的可用子命令 |
| 消息范围不符 | 提示“此命令只能在群聊中使用”或对应私聊提示 |
| 权限不足 | 提示“管理员专用命令，普通用户无法使用” |
| 缺少参数 | 显示参数名和该命令用法 |
| 参数格式错误 | 显示参数名和该命令用法，不展示 Rust 解析错误 |
| 参数过多 | 提示参数过多并显示用法 |
| `CommandError::User` | 回复业务提供的安全文本 |
| `CommandError::Internal` | 记录完整日志，回复“命令执行失败，请稍后重试” |

注册阶段验证以下开发配置错误：

- 重复根命令。
- 同级节点名称重复。
- 同级名称和别名冲突。
- 子节点放宽父节点权限。
- 没有处理函数也没有子节点的不可执行叶子节点。

`install()` 返回 `CommandRegistrationError`。插件在启动阶段使用 `?` 或带上下文的 `expect` 暴露错误，不将其转换为聊天消息。

## 帮助目录

`CommandRouter::install()` 将不含闭包的只读元数据写入 `CommandCatalog`：

```rust
CommandMetadata {
    owner: "bilibili",
    path: vec!["/live", "add"],
    aliases: vec![],
    description: "为本群添加直播订阅",
    usage: "/live add <uid>",
    scope: MessageScope::Group,
    permission: Permission::BotAdmin,
}
```

`help_msg` 自身也通过 `CommandRouter` 注册 `/help`：

- `/help` 列出所有根命令及说明。
- `/help live` 和 `/help /live` 展示 `/live` 说明及直接子命令。
- `/help live add` 展示具体用法、权限和别名。
- `/help 今日发言排行` 和 `/help #今日发言排行` 等价。
- 查询不存在的路径时回复“帮助信息不存在”。

父节点帮助由命令树自动生成，叶子节点使用注册时提供的 `usage`。命令名称、别名、权限和帮助信息只有一个数据来源。

目录项按 `owner + root` 更新，使同一插件重载时替换旧元数据。第一版保持当前帮助注册表的生命周期语义：插件被停用后，目录项可能保留到进程重启；实际命令监听器仍由 Kovi 停用。插件生命周期自动注销不在本次范围内。

## 现有命令迁移

迁移以下自研命令：

| 命令 | 范围 | 权限 |
| --- | --- | --- |
| `/help [path...]` | `Any` | `Everyone` |
| `!md <content>` | `Any` | `Everyone` |
| `/查卡 <card_name>` | `Any` | `Everyone` |
| `#今日发言排行` | `Group` | `Everyone` |
| `/wordcloud once` | `Group` | `BotAdmin` |
| `/wordcloud enable` | `Group` | `BotAdmin` |
| `/wordcloud disable` | `Group` | `BotAdmin` |
| `/wordcloud status` | `Group` | `BotAdmin` |
| `/live list` | `Group` | `Everyone` |
| `/live add <uid>` | `Group` | `BotAdmin` |
| `/live rm <uid>` | `Group` | `BotAdmin` |
| `/live remove <uid>` | `Group` | `BotAdmin`，作为 `rm` 的别名 |
| `/dynamic list` | `Group` | `Everyone` |
| `/dynamic add <uid>` | `Group` | `BotAdmin` |
| `/dynamic rm <uid>` | `Group` | `BotAdmin` |
| `/dynamic fetch <uid> [count]` | `Group` | `BotAdmin` |

边界行为统一为：

- `/livefoo` 静默忽略。
- `/live` 展示可用子命令。
- `/wordcloud` 展示用法和可用子命令。
- `/查卡` 提示缺少卡片名称。
- `!md` 提示缺少 Markdown 内容。
- `/live remove <uid>` 按 `/live rm <uid>` 执行。

迁移后删除所有 `help_msg::register_help` 调用，并移除业务插件对 `help_msg` 的依赖。

## 测试策略

`plugins/utils/tests/command.rs` 至少覆盖：

- 根命令完整匹配以及未注册根命令返回 `Ignored`。
- 一级和三级子命令的最长路径匹配。
- 主名称和别名得到相同匹配节点。
- Unicode 空白分词。
- `rest()` 保留 Markdown 缩进，`trimmed_rest()` 去除首尾空白。
- Kovi 消息文本段适配不裁剪 `rest()` 的首尾内容。
- 中间节点缺少子命令和未知子命令。
- `Everyone` 与 `BotAdmin` 权限判定。
- 权限继承、收紧以及非法放宽的注册错误。
- `Any`、`Group` 和 `Private` 范围校验。
- 缺失参数、格式错误、多余参数和内部错误的统一回复。
- 重复根命令、重复同级名称和别名冲突。
- 权限或范围校验失败时处理函数不会执行。

各业务插件保留或增加针对业务参数边界的测试。最终验证运行：

```bash
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

## 实施顺序

1. 在 `utils` 中实现命令树、纯解析核心、权限和错误模型及单元测试。
2. 实现 Kovi `MsgEvent` 适配和每插件 `CommandRouter::install()`。
3. 将 `help_msg` 改为读取 `CommandCatalog` 并通过路由器注册 `/help`。
4. 迁移 `/help`、`!md` 和 `/查卡`。
5. 迁移 `#今日发言排行` 和 `/wordcloud`。
6. 迁移 `/live` 和 `/dynamic`。
7. 删除重复帮助注册和不再需要的依赖。
8. 格式化并运行工作区全部静态检查与测试。
