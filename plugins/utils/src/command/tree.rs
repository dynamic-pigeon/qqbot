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
        let root_token = &input[root_span.clone()];
        let Some(hit) = match_root_facing(&self.roots, root_token) else {
            return ResolveOutcome::Ignored;
        };

        let mut node = hit.node;
        let mut path = hit.path;
        let mut consumed = 1;
        let mut glue: Option<(String, usize)> = None;

        if hit.matched_len < root_token.len() {
            glue = Some((
                root_token[hit.matched_len..].to_owned(),
                root_span.start + hit.matched_len,
            ));
        } else {
            while let Some(span) = spans.get(consumed) {
                let word = &input[span.clone()];
                let Some((child, matched_len)) = match_siblings(&node.children, word) else {
                    break;
                };
                node = child;
                path.push(node.name.clone());
                consumed += 1;
                if matched_len < word.len() {
                    glue = Some((word[matched_len..].to_owned(), span.start + matched_len));
                    break;
                }
            }
        }

        // 有子命令时，对不上的下一个词（含粘连前缀剩下的部分）一律当未知子命令，
        // 避免父节点默认 handler 把「图库 乱输」吃成参数过多。
        if !node.children.is_empty() {
            if let Some((suffix, _)) = glue.as_ref() {
                return unknown_subcommand(node, path, suffix.clone());
            }
            if let Some(span) = spans.get(consumed) {
                return unknown_subcommand(node, path, input[span.clone()].to_owned());
            }
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

        let (args, rest) = if let Some((suffix, rest_start)) = glue {
            let mut args = vec![suffix];
            args.extend(
                spans[consumed..]
                    .iter()
                    .map(|span| input[span.clone()].to_owned()),
            );
            (args, input[rest_start..].to_owned())
        } else {
            let args = spans[consumed..]
                .iter()
                .map(|span| input[span.clone()].to_owned())
                .collect();
            (args, rest_after_path(input, &spans, consumed))
        };

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

fn token_match(command: &Command, token: &str) -> Option<usize> {
    let mut best = None;
    for name in std::iter::once(&command.name).chain(&command.aliases) {
        let matched = if command.prefix_match {
            !name.is_empty() && token.starts_with(name.as_str())
        } else {
            token == name
        };
        if matched {
            best = Some(best.map_or(name.len(), |best: usize| best.max(name.len())));
        }
    }
    best
}

fn match_siblings<'a>(commands: &'a [Command], token: &str) -> Option<(&'a Command, usize)> {
    let mut best: Option<(&Command, usize)> = None;
    for command in commands {
        if let Some(len) = token_match(command, token)
            && best.is_none_or(|(_, best_len)| len > best_len)
        {
            best = Some((command, len));
        }
    }
    best
}

struct RootHit<'a> {
    node: &'a Command,
    path: Vec<String>,
    matched_len: usize,
}

fn match_root_facing<'a>(roots: &'a [Command], token: &str) -> Option<RootHit<'a>> {
    let mut best: Option<RootHit<'a>> = None;

    fn consider<'a>(
        best: &mut Option<RootHit<'a>>,
        node: &'a Command,
        path: Vec<String>,
        token: &str,
    ) {
        if let Some(len) = token_match(node, token)
            && best
                .as_ref()
                .is_none_or(|current| len > current.matched_len)
        {
            *best = Some(RootHit {
                node,
                path,
                matched_len: len,
            });
        }
    }

    fn walk_exposed<'a>(
        best: &mut Option<RootHit<'a>>,
        node: &'a Command,
        path: &[String],
        token: &str,
    ) {
        for child in &node.children {
            let mut child_path = path.to_vec();
            child_path.push(child.name.clone());
            if child.expose_as_root {
                consider(best, child, child_path.clone(), token);
            }
            walk_exposed(best, child, &child_path, token);
        }
    }

    for root in roots {
        let path = vec![root.name.clone()];
        consider(&mut best, root, path.clone(), token);
        walk_exposed(&mut best, root, &path, token);
    }
    best
}

fn unknown_subcommand(node: &Command, path: Vec<String>, subcommand: String) -> ResolveOutcome {
    ResolveOutcome::Error(RouteError::UnknownSubcommand {
        path,
        subcommand,
        usage: node.usage.clone(),
        available: node
            .children
            .iter()
            .map(|child| child.name.clone())
            .collect(),
        permission: node.permission.unwrap_or_default(),
        scope: node.scope.unwrap_or_default(),
    })
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
