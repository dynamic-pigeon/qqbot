use std::{collections::HashMap, fmt, ops::Range};

use super::model::{Command, CommandArguments, CommandHandler, MessageScope, Permission};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommandRegistrationError {
    #[error("命令名称不能为空，父路径: {parent_path}")]
    EmptyName { parent_path: String },
    #[error("命令名称或别名 `{name}` 在 `{parent_path}` 下重复")]
    DuplicateName { parent_path: String, name: String },
    #[error("命令叶子 `{path}` 没有处理函数")]
    EmptyLeaf { path: String },
    #[error("子命令 `{path}` 不能放宽父命令的管理员权限")]
    PermissionRelaxation { path: String },
    #[error("根命令 `{root}` 已由插件 `{owner}` 注册")]
    RootConflict { root: String, owner: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RouteError {
    MissingSubcommand {
        path: Vec<String>,
        usage: String,
        available: Vec<String>,
        permission: Permission,
        scope: MessageScope,
    },
    UnknownSubcommand {
        path: Vec<String>,
        subcommand: String,
        usage: String,
        available: Vec<String>,
        permission: Permission,
        scope: MessageScope,
    },
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSubcommand {
                usage, available, ..
            } => write_usage(formatter, "请指定子命令", usage, available),
            Self::UnknownSubcommand {
                subcommand,
                usage,
                available,
                ..
            } => write_usage(
                formatter,
                &format!("未知子命令 `{subcommand}`"),
                usage,
                available,
            ),
        }
    }
}

impl RouteError {
    pub fn permission(&self) -> Permission {
        match self {
            Self::MissingSubcommand { permission, .. }
            | Self::UnknownSubcommand { permission, .. } => *permission,
        }
    }

    pub fn scope(&self) -> MessageScope {
        match self {
            Self::MissingSubcommand { scope, .. } | Self::UnknownSubcommand { scope, .. } => *scope,
        }
    }
}

fn write_usage(
    formatter: &mut fmt::Formatter<'_>,
    message: &str,
    usage: &str,
    available: &[String],
) -> fmt::Result {
    write!(formatter, "{message}")?;
    if !usage.is_empty() {
        write!(formatter, "\n用法: {usage}")?;
    }
    if !available.is_empty() {
        write!(formatter, "\n可用子命令: {}", available.join(" | "))?;
    }
    Ok(())
}

pub enum ResolveOutcome {
    Ignored,
    Matched(ResolvedCommand),
    Error(RouteError),
}

pub struct ResolvedCommand {
    pub(crate) path: Vec<String>,
    pub(crate) args: Vec<String>,
    pub(crate) rest: String,
    pub(crate) usage: String,
    pub(crate) permission: Permission,
    pub(crate) scope: MessageScope,
    pub(crate) handler: CommandHandler,
}

impl ResolvedCommand {
    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn rest(&self) -> &str {
        &self.rest
    }

    pub fn trimmed_rest(&self) -> &str {
        self.rest.trim()
    }

    pub fn usage(&self) -> &str {
        &self.usage
    }

    pub fn permission(&self) -> Permission {
        self.permission
    }

    pub fn scope(&self) -> MessageScope {
        self.scope
    }

    pub(crate) fn into_dispatch_parts(
        self,
    ) -> (
        Vec<String>,
        CommandArguments,
        String,
        Permission,
        MessageScope,
        CommandHandler,
    ) {
        (
            self.path,
            CommandArguments::new(self.args, self.rest),
            self.usage,
            self.permission,
            self.scope,
            self.handler,
        )
    }
}

pub struct CommandTree {
    roots: Vec<Command>,
}

impl CommandTree {
    pub fn new(mut roots: Vec<Command>) -> Result<Self, CommandRegistrationError> {
        validate_siblings(&roots, &[])?;
        validate_exposed_root_names(&roots)?;
        for root in &mut roots {
            prepare_node(root, Permission::Everyone, MessageScope::Any, &[])?;
        }
        Ok(Self { roots })
    }

    pub fn resolve(&self, input: &str) -> ResolveOutcome {
        let input = input.trim_start();
        let spans = word_spans(input);
        let Some(root_span) = spans.first() else {
            return ResolveOutcome::Ignored;
        };
        let root_name = &input[root_span.clone()];
        let (mut node, mut path) =
            if let Some(root) = self.roots.iter().find(|node| node.matches(root_name)) {
                (root, vec![root.name.clone()])
            } else if let Some(exposed) = find_exposed_root(&self.roots, root_name) {
                exposed
            } else {
                return ResolveOutcome::Ignored;
            };

        let mut consumed = 1;
        while let Some(span) = spans.get(consumed) {
            let word = &input[span.clone()];
            let Some(child) = node.children.iter().find(|child| child.matches(word)) else {
                break;
            };
            node = child;
            path.push(node.name.clone());
            consumed += 1;
        }

        let Some(handler) = node.handler.clone() else {
            let usage = node.usage.clone();
            let permission = node.permission.unwrap_or_default();
            let scope = node.scope.unwrap_or_default();
            let available = node
                .children
                .iter()
                .map(|child| child.name.clone())
                .collect();
            return match spans.get(consumed) {
                Some(span) => ResolveOutcome::Error(RouteError::UnknownSubcommand {
                    path,
                    subcommand: input[span.clone()].to_owned(),
                    usage,
                    available,
                    permission,
                    scope,
                }),
                None => ResolveOutcome::Error(RouteError::MissingSubcommand {
                    path,
                    usage,
                    available,
                    permission,
                    scope,
                }),
            };
        };

        let args = spans[consumed..]
            .iter()
            .map(|span| input[span.clone()].to_owned())
            .collect();
        let rest = rest_after_path(input, &spans, consumed);

        ResolveOutcome::Matched(ResolvedCommand {
            path,
            args,
            rest,
            usage: node.usage.clone(),
            permission: node.permission.unwrap_or_default(),
            scope: node.scope.unwrap_or_default(),
            handler,
        })
    }

    pub(crate) fn roots(&self) -> &[Command] {
        &self.roots
    }
}

impl Command {
    fn matches(&self, word: &str) -> bool {
        self.name == word || self.aliases.iter().any(|alias| alias == word)
    }
}

fn prepare_node(
    node: &mut Command,
    inherited_permission: Permission,
    inherited_scope: MessageScope,
    parent_path: &[String],
) -> Result<(), CommandRegistrationError> {
    if node.name.is_empty() {
        return Err(CommandRegistrationError::EmptyName {
            parent_path: display_path(parent_path),
        });
    }

    let mut path = parent_path.to_vec();
    path.push(node.name.clone());
    if inherited_permission == Permission::BotAdmin && node.permission == Some(Permission::Everyone)
    {
        return Err(CommandRegistrationError::PermissionRelaxation {
            path: display_path(&path),
        });
    }
    if node.handler.is_none() && node.children.is_empty() {
        return Err(CommandRegistrationError::EmptyLeaf {
            path: display_path(&path),
        });
    }

    let permission = node.permission.unwrap_or(inherited_permission);
    let scope = node.scope.unwrap_or(inherited_scope);
    node.permission = Some(permission);
    node.scope = Some(scope);
    validate_siblings(&node.children, &path)?;
    for child in &mut node.children {
        prepare_node(child, permission, scope, &path)?;
    }
    Ok(())
}

fn find_exposed_root<'a>(roots: &'a [Command], name: &str) -> Option<(&'a Command, Vec<String>)> {
    fn walk<'a>(node: &'a Command, name: &str, path: &mut Vec<String>) -> Option<&'a Command> {
        for child in &node.children {
            path.push(child.name.clone());
            if child.expose_as_root && child.matches(name) {
                return Some(child);
            }
            if let Some(found) = walk(child, name, path) {
                return Some(found);
            }
            path.pop();
        }
        None
    }

    for root in roots {
        let mut path = vec![root.name.clone()];
        if let Some(found) = walk(root, name, &mut path) {
            return Some((found, path));
        }
    }
    None
}

fn validate_exposed_root_names(roots: &[Command]) -> Result<(), CommandRegistrationError> {
    let mut claimed = HashMap::new();
    for root in roots {
        for name in std::iter::once(&root.name).chain(&root.aliases) {
            claimed.insert(name.as_str(), root.name.as_str());
        }
    }
    for root in roots {
        claim_exposed_root_names(root, &mut claimed)?;
    }
    Ok(())
}

fn claim_exposed_root_names<'a>(
    command: &'a Command,
    claimed: &mut HashMap<&'a str, &'a str>,
) -> Result<(), CommandRegistrationError> {
    for child in &command.children {
        if child.expose_as_root {
            for name in std::iter::once(&child.name).chain(&child.aliases) {
                if claimed.insert(name.as_str(), child.name.as_str()).is_some() {
                    return Err(CommandRegistrationError::DuplicateName {
                        parent_path: "<root>".to_owned(),
                        name: name.clone(),
                    });
                }
            }
        }
        claim_exposed_root_names(child, claimed)?;
    }
    Ok(())
}

fn validate_siblings(
    commands: &[Command],
    parent_path: &[String],
) -> Result<(), CommandRegistrationError> {
    let mut names = HashMap::new();
    for command in commands {
        for name in std::iter::once(&command.name).chain(&command.aliases) {
            if names.insert(name.as_str(), command.name.as_str()).is_some() {
                return Err(CommandRegistrationError::DuplicateName {
                    parent_path: display_path(parent_path),
                    name: name.clone(),
                });
            }
        }
    }
    Ok(())
}

fn display_path(path: &[String]) -> String {
    if path.is_empty() {
        "<root>".to_owned()
    } else {
        path.join(" ")
    }
}

fn word_spans(input: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;

    for (index, character) in input.char_indices() {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                spans.push(start..index);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        spans.push(start..input.len());
    }
    spans
}

fn rest_after_path(input: &str, spans: &[Range<usize>], consumed: usize) -> String {
    let Some(path_end) = consumed
        .checked_sub(1)
        .and_then(|index| spans.get(index))
        .map(|span| span.end)
    else {
        return String::new();
    };
    let suffix = &input[path_end..];
    let rest = suffix
        .chars()
        .next()
        .filter(|character| character.is_whitespace())
        .map_or(suffix, |character| &suffix[character.len_utf8()..]);
    rest.to_owned()
}
