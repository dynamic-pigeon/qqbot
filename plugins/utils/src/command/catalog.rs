use std::{
    collections::BTreeMap,
    fmt::Write as _,
    sync::{LazyLock, RwLock},
};

use super::{Command, CommandRegistrationError, CommandTree, MessageScope, Permission};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandMetadata {
    pub path: Vec<String>,
    pub aliases: Vec<String>,
    pub description: String,
    pub usage: String,
    pub scope: MessageScope,
    pub permission: Permission,
    pub group: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandHelp {
    pub command: CommandMetadata,
    pub children: Vec<CommandMetadata>,
}

#[derive(Default)]
pub(crate) struct CatalogStore {
    plugins: Vec<CatalogPlugin>,
}

struct CatalogPlugin {
    owner: String,
    tree: CommandTree,
}

impl CatalogStore {
    pub fn register(
        &mut self,
        owner: &str,
        tree: &CommandTree,
    ) -> Result<(), CommandRegistrationError> {
        let incoming = tree.root_facing_names();
        for plugin in &self.plugins {
            if plugin.owner == owner {
                continue;
            }
            for name in plugin.tree.root_facing_names() {
                if incoming.contains(&name) {
                    return Err(CommandRegistrationError::RootConflict {
                        root: name.to_owned(),
                        owner: plugin.owner.clone(),
                    });
                }
            }
        }

        self.plugins.retain(|plugin| plugin.owner != owner);
        self.plugins.push(CatalogPlugin {
            owner: owner.to_owned(),
            tree: tree.clone(),
        });
        Ok(())
    }

    pub fn unregister(&mut self, owner: &str) {
        self.plugins.retain(|plugin| plugin.owner != owner);
    }

    /// 按注册顺序返回各插件的根命令；展示层的排序在 render_help 内做。
    pub fn roots(&self) -> Vec<CommandMetadata> {
        self.plugins
            .iter()
            .flat_map(|plugin| {
                plugin
                    .tree
                    .roots()
                    .iter()
                    .map(|root| metadata(vec![root.name.clone()], root))
            })
            .collect()
    }

    pub fn find(&self, path: &[&str]) -> Option<CommandHelp> {
        self.plugins.iter().find_map(|plugin| {
            plugin
                .tree
                .find_for_help(path)
                .map(|(command, canonical)| help_of(canonical, command))
        })
    }

    pub fn render_help(&self, path: &[&str]) -> String {
        let (grouped, loose) = self.partition();
        if path.is_empty() {
            if grouped.is_empty() && loose.is_empty() {
                return "暂无帮助信息".to_owned();
            }

            // 分组在顶层只占一行入口，成员经 `/help <标签>` 展开；
            // 未分组的保持「名: 描述」条目，按名字排序。
            let mut output = String::from("📚 可用命令:\n");
            for (label, members) in &grouped {
                let _ = writeln!(
                    output,
                    "• 【{label}】{} 条: /help {label} 查看",
                    members.len()
                );
            }
            for root in &loose {
                write_root_line(&mut output, root);
            }
            output.push_str("\n使用 `/help <命令路径>` 查看详细用法");
            return output;
        }

        if let [label] = path {
            let label = *label;
            if let Some(members) = grouped.get(label) {
                let mut output = format!("📖 【{label}】\n");
                for root in members {
                    write_root_line(&mut output, root);
                }
                output.push_str("\n使用 `/help <命令路径>` 查看详细用法");
                return output;
            }
        }

        let Some(help) = self.find(path) else {
            return format!("命令 `{}` 的帮助信息不存在", path.join(" "));
        };
        render_command_help(&help)
    }

    /// 根命令按分组标签聚拢（组间按标签排序，组内保持注册顺序），
    /// 未分组的单独返回并按名字排序。
    fn partition(&self) -> (BTreeMap<String, Vec<CommandMetadata>>, Vec<CommandMetadata>) {
        let mut grouped: BTreeMap<String, Vec<CommandMetadata>> = BTreeMap::new();
        let mut loose = Vec::new();
        for root in self.roots() {
            match root.group {
                Some(ref label) => grouped.entry(label.clone()).or_default().push(root),
                None => loose.push(root),
            }
        }
        loose.sort_by(|left, right| left.path[0].cmp(&right.path[0]));
        (grouped, loose)
    }
}

static COMMAND_CATALOG: LazyLock<RwLock<CatalogStore>> =
    LazyLock::new(|| RwLock::new(CatalogStore::default()));

pub struct CommandCatalog;

impl CommandCatalog {
    pub fn render_help(path: &[&str]) -> String {
        COMMAND_CATALOG
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .render_help(path)
    }

    pub(crate) fn register(
        owner: &str,
        tree: &CommandTree,
    ) -> Result<(), CommandRegistrationError> {
        COMMAND_CATALOG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .register(owner, tree)
    }

    pub(crate) fn unregister(owner: &str) {
        COMMAND_CATALOG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unregister(owner);
    }
}

fn metadata(path: Vec<String>, command: &Command) -> CommandMetadata {
    CommandMetadata {
        path,
        aliases: command.aliases.clone(),
        description: command.description.clone(),
        usage: command.usage.clone(),
        scope: command.scope.unwrap_or_default(),
        permission: command.permission.unwrap_or_default(),
        group: command.group.clone(),
    }
}

fn help_of(path: Vec<String>, command: &Command) -> CommandHelp {
    let children = command
        .children
        .iter()
        .map(|child| {
            let mut child_path = path.clone();
            child_path.push(child.name.clone());
            metadata(child_path, child)
        })
        .collect();
    CommandHelp {
        command: metadata(path, command),
        children,
    }
}

fn render_command_help(help: &CommandHelp) -> String {
    let command = &help.command;
    let mut output = format!("📖 `{}`", command.path.join(" "));
    if !command.description.is_empty() {
        let _ = write!(output, "\n{}", command.description);
    }
    if !command.usage.is_empty() {
        let _ = write!(output, "\n用法: {}", command.usage);
    }
    if !command.aliases.is_empty() {
        let _ = write!(output, "\n别名: {}", command.aliases.join(" | "));
    }
    let _ = write!(
        output,
        "\n权限: {}\n范围: {}",
        permission_label(command.permission),
        scope_label(command.scope)
    );
    if !help.children.is_empty() {
        output.push_str("\n可用子命令:");
        for child in &help.children {
            let name = child.path.last().map_or("", String::as_str);
            let _ = write!(output, "\n• `{name}`: {}", child.description);
        }
    }
    output
}

fn write_root_line(output: &mut String, root: &CommandMetadata) {
    let _ = writeln!(output, "• `{}`: {}", root.path[0], root.description);
}

fn permission_label(permission: Permission) -> &'static str {
    match permission {
        Permission::Everyone => "所有用户",
        Permission::BotAdmin => "机器人管理员",
    }
}

fn scope_label(scope: MessageScope) -> &'static str {
    match scope {
        MessageScope::Any => "群聊或私聊",
        MessageScope::Group => "群聊",
        MessageScope::Private => "私聊",
    }
}
