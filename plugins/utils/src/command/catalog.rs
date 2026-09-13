use std::{
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

    pub fn roots(&self) -> Vec<CommandMetadata> {
        let mut roots: Vec<_> = self
            .plugins
            .iter()
            .flat_map(|plugin| {
                plugin
                    .tree
                    .roots()
                    .iter()
                    .map(|root| metadata(vec![root.name.clone()], root))
            })
            .collect();
        roots.sort_by(|left, right| left.path[0].cmp(&right.path[0]));
        roots
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
        if path.is_empty() {
            let roots = self.roots();
            if roots.is_empty() {
                return "暂无帮助信息".to_owned();
            }

            let mut output = String::from("📚 可用命令:\n");
            for root in roots {
                let _ = writeln!(output, "• `{}`: {}", root.path[0], root.description);
            }
            output.push_str("\n使用 `/help <命令路径>` 查看详细用法");
            return output;
        }

        let Some(help) = self.find(path) else {
            return format!("命令 `{}` 的帮助信息不存在", path.join(" "));
        };
        render_command_help(&help)
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
}

fn metadata(path: Vec<String>, command: &Command) -> CommandMetadata {
    CommandMetadata {
        path,
        aliases: command.aliases.clone(),
        description: command.description.clone(),
        usage: command.usage.clone(),
        scope: command.scope.unwrap_or_default(),
        permission: command.permission.unwrap_or_default(),
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
