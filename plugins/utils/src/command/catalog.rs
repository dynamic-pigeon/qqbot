use std::{collections::BTreeMap, fmt::Write as _};

use std::sync::{LazyLock, RwLock};

use super::{Command, CommandRegistrationError, CommandTree, MessageScope, Permission};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommandMetadata {
    pub owner: String,
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
    roots: BTreeMap<String, CatalogRoot>,
}

struct CatalogRoot {
    owner: String,
    entries: Vec<CatalogEntry>,
}

struct CatalogEntry {
    metadata: CommandMetadata,
    matchers: Vec<Vec<String>>,
    expose_as_root: bool,
}

impl CatalogStore {
    pub fn register(
        &mut self,
        owner: &str,
        tree: &CommandTree,
    ) -> Result<(), CommandRegistrationError> {
        for name in tree_root_facing_names(tree) {
            if let Some(existing) = self
                .roots
                .values()
                .find(|existing| existing.owner != owner && occupies_root_name(existing, name))
            {
                return Err(CommandRegistrationError::RootConflict {
                    root: name.to_owned(),
                    owner: existing.owner.clone(),
                });
            }
        }

        for root in tree.roots() {
            let mut entries = Vec::new();
            collect_entries(owner, root, &[], &[], &mut entries);
            self.roots.insert(
                root.name.clone(),
                CatalogRoot {
                    owner: owner.to_owned(),
                    entries,
                },
            );
        }
        Ok(())
    }

    pub fn roots(&self) -> Vec<CommandMetadata> {
        self.roots
            .values()
            .filter_map(|root| root.entries.first())
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    pub fn find(&self, path: &[&str]) -> Option<CommandHelp> {
        if path.is_empty() {
            return None;
        }
        self.find_canonical(path)
            .or_else(|| self.find_exposed(path))
    }

    fn find_canonical(&self, path: &[&str]) -> Option<CommandHelp> {
        self.roots.values().find_map(|root| {
            root.entries
                .iter()
                .find(|entry| path_matches(&entry.matchers, path))
                .map(|entry| help_from_entry(root, entry))
        })
    }

    fn find_exposed(&self, path: &[&str]) -> Option<CommandHelp> {
        let (first, rest) = path.split_first()?;
        let (root, entry) = self.roots.values().find_map(|root| {
            root.entries
                .iter()
                .find(|entry| {
                    entry.expose_as_root
                        && entry.matchers.last().is_some_and(|names| {
                            names.iter().any(|name| root_name_matches(name, first))
                        })
                })
                .map(|entry| (root, entry))
        })?;
        if rest.is_empty() {
            return Some(help_from_entry(root, entry));
        }

        let canonical_path = &entry.metadata.path;
        root.entries
            .iter()
            .find(|candidate| {
                candidate.metadata.path.len() == canonical_path.len() + rest.len()
                    && candidate.metadata.path.starts_with(canonical_path)
                    && candidate.metadata.path[canonical_path.len()..]
                        .iter()
                        .zip(rest)
                        .all(|(name, part)| name == part)
            })
            .map(|entry| help_from_entry(root, entry))
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
        read_catalog().render_help(path)
    }

    pub(crate) fn register(
        owner: &str,
        tree: &CommandTree,
    ) -> Result<(), CommandRegistrationError> {
        let mut catalog = COMMAND_CATALOG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        catalog.register(owner, tree)
    }
}

fn read_catalog() -> std::sync::RwLockReadGuard<'static, CatalogStore> {
    COMMAND_CATALOG
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn collect_entries(
    owner: &str,
    command: &Command,
    parent_path: &[String],
    parent_matchers: &[Vec<String>],
    entries: &mut Vec<CatalogEntry>,
) {
    let mut path = parent_path.to_vec();
    path.push(command.name.clone());
    let mut matchers = parent_matchers.to_vec();
    let mut names = Vec::with_capacity(command.aliases.len() + 1);
    names.push(command.name.clone());
    names.extend(command.aliases.iter().cloned());
    matchers.push(names);

    entries.push(CatalogEntry {
        metadata: CommandMetadata {
            owner: owner.to_owned(),
            path: path.clone(),
            aliases: command.aliases.clone(),
            description: command.description.clone(),
            usage: command.usage.clone(),
            scope: command.scope.unwrap_or_default(),
            permission: command.permission.unwrap_or_default(),
        },
        matchers: matchers.clone(),
        expose_as_root: command.expose_as_root,
    });
    for child in &command.children {
        collect_entries(owner, child, &path, &matchers, entries);
    }
}

fn help_from_entry(root: &CatalogRoot, entry: &CatalogEntry) -> CommandHelp {
    let canonical_path = &entry.metadata.path;
    let children = root
        .entries
        .iter()
        .filter(|candidate| {
            candidate.metadata.path.len() == canonical_path.len() + 1
                && candidate.metadata.path.starts_with(canonical_path)
        })
        .map(|candidate| candidate.metadata.clone())
        .collect();

    CommandHelp {
        command: entry.metadata.clone(),
        children,
    }
}

fn path_matches(matchers: &[Vec<String>], path: &[&str]) -> bool {
    matchers.len() == path.len()
        && matchers
            .iter()
            .enumerate()
            .zip(path)
            .all(|((index, names), part)| {
                names.iter().any(|name| {
                    if index == 0 {
                        root_name_matches(name, part)
                    } else {
                        name == part
                    }
                })
            })
}

fn tree_root_facing_names(tree: &CommandTree) -> Vec<&str> {
    let mut names = Vec::new();
    fn walk<'a>(command: &'a Command, is_root: bool, names: &mut Vec<&'a str>) {
        if is_root || command.expose_as_root {
            names.push(command.name.as_str());
            names.extend(command.aliases.iter().map(String::as_str));
        }
        for child in &command.children {
            walk(child, false, names);
        }
    }
    for root in tree.roots() {
        walk(root, true, &mut names);
    }
    names
}

fn occupies_root_name(root: &CatalogRoot, name: &str) -> bool {
    root.entries.iter().any(|entry| {
        let faces_root = entry.metadata.path.len() == 1 || entry.expose_as_root;
        faces_root
            && entry
                .matchers
                .last()
                .is_some_and(|names| names.iter().any(|registered| registered == name))
    })
}

fn root_name_matches(registered: &str, requested: &str) -> bool {
    registered == requested
        || registered
            .strip_prefix(['/', '!', '#'])
            .is_some_and(|name| name == requested)
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
