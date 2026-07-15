use std::{future::Future, pin::Pin, str::FromStr, sync::Arc};

use kovi::{Message, RuntimeBot};
use kovi_onebot::{MsgEvent, RepliableEvent};

pub type CommandResult = Result<(), CommandError>;
pub(crate) type CommandFuture = Pin<Box<dyn Future<Output = CommandResult> + Send>>;
pub(crate) type CommandHandler = Arc<dyn Fn(CommandContext) -> CommandFuture + Send + Sync>;

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

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("缺少参数 `{name}`")]
    MissingArgument { name: String },
    #[error("参数 `{name}` 格式错误")]
    InvalidArgument { name: String },
    #[error("参数过多")]
    UnexpectedArgument,
    #[error("{0}")]
    User(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl CommandError {
    pub fn user(message: impl Into<String>) -> Self {
        Self::User(message.into())
    }

    pub fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandArguments {
    args: Vec<String>,
    rest: String,
}

impl CommandArguments {
    pub fn new(args: Vec<String>, rest: String) -> Self {
        Self { args, rest }
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }

    pub fn arg(&self, index: usize) -> Option<&str> {
        self.args.get(index).map(String::as_str)
    }

    pub fn parse_arg<T>(&self, index: usize, name: &str) -> Result<T, CommandError>
    where
        T: FromStr,
    {
        self.arg(index)
            .ok_or_else(|| CommandError::MissingArgument {
                name: name.to_owned(),
            })?
            .parse()
            .map_err(|_| CommandError::InvalidArgument {
                name: name.to_owned(),
            })
    }

    pub fn ensure_no_extra_args(&self, expected: usize) -> Result<(), CommandError> {
        if self.args.len() > expected {
            Err(CommandError::UnexpectedArgument)
        } else {
            Ok(())
        }
    }

    pub fn rest(&self) -> &str {
        &self.rest
    }

    pub fn trimmed_rest(&self) -> &str {
        self.rest.trim()
    }
}

pub fn render_command_error(error: &CommandError, usage: &str) -> String {
    let message = match error {
        CommandError::MissingArgument { name } => format!("缺少参数 `{name}`"),
        CommandError::InvalidArgument { name } => format!("参数 `{name}` 格式错误"),
        CommandError::UnexpectedArgument => "参数过多".to_owned(),
        CommandError::User(message) => return message.clone(),
        CommandError::Internal(_) => return "命令执行失败，请稍后重试".to_owned(),
    };

    if usage.is_empty() {
        message
    } else {
        format!("{message}\n用法: {usage}")
    }
}

#[derive(Clone)]
pub struct CommandContext {
    event: Arc<MsgEvent>,
    bot: Arc<RuntimeBot>,
    path: Vec<String>,
    arguments: CommandArguments,
}

impl CommandContext {
    pub(crate) fn new(
        event: Arc<MsgEvent>,
        bot: Arc<RuntimeBot>,
        path: Vec<String>,
        arguments: CommandArguments,
    ) -> Self {
        Self {
            event,
            bot,
            path,
            arguments,
        }
    }

    pub fn event(&self) -> &MsgEvent {
        &self.event
    }

    pub fn bot(&self) -> &Arc<RuntimeBot> {
        &self.bot
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn args(&self) -> &[String] {
        self.arguments.args()
    }

    pub fn arg(&self, index: usize) -> Option<&str> {
        self.arguments.arg(index)
    }

    pub fn parse_arg<T>(&self, index: usize, name: &str) -> Result<T, CommandError>
    where
        T: FromStr,
    {
        self.arguments.parse_arg(index, name)
    }

    pub fn ensure_no_extra_args(&self, expected: usize) -> Result<(), CommandError> {
        self.arguments.ensure_no_extra_args(expected)
    }

    pub fn rest(&self) -> &str {
        self.arguments.rest()
    }

    pub fn trimmed_rest(&self) -> &str {
        self.arguments.trimmed_rest()
    }

    pub fn reply<T>(&self, message: T)
    where
        Message: From<T>,
    {
        let message = Message::from(message);
        RepliableEvent::reply::<Message>(self.event.as_ref(), message);
    }

    pub fn reply_and_quote<T>(&self, message: T)
    where
        Message: From<T>,
    {
        let message = Message::from(message);
        RepliableEvent::reply_and_quote::<Message>(self.event.as_ref(), message);
    }
}

pub struct Command {
    pub(crate) name: String,
    pub(crate) aliases: Vec<String>,
    pub(crate) description: String,
    pub(crate) usage: String,
    pub(crate) permission: Option<Permission>,
    pub(crate) scope: Option<MessageScope>,
    pub(crate) children: Vec<Command>,
    pub(crate) handler: Option<CommandHandler>,
}

impl Command {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            description: String::new(),
            usage: String::new(),
            permission: None,
            scope: None,
            children: Vec::new(),
            handler: None,
        }
    }

    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn usage(mut self, usage: impl Into<String>) -> Self {
        self.usage = usage.into();
        self
    }

    pub fn permission(mut self, permission: Permission) -> Self {
        self.permission = Some(permission);
        self
    }

    pub fn scope(mut self, scope: MessageScope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn subcommand(mut self, child: Command) -> Self {
        self.children.push(child);
        self
    }

    pub fn handler<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = CommandResult> + Send + 'static,
    {
        self.handler = Some(Arc::new(move |context| Box::pin(handler(context))));
        self
    }
}
