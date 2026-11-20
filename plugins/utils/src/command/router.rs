use std::sync::Arc;

use kovi::{Message, PluginBuilder as plugin, RuntimeBot};

use crate::bot::{self, BotMsgEvent};

use super::model::{CommandArguments, CommandHandler};
use super::{
    AccessError, Command, CommandCatalog, CommandContext, CommandError, CommandRegistrationError,
    CommandTree, MessageSource, Permission, ResolveOutcome, ResolvedCommand, check_access,
    render_command_error, user_is_bot_admin,
};

pub struct CommandRouter {
    owner: String,
    bot: Arc<RuntimeBot>,
    commands: Vec<Command>,
}

impl CommandRouter {
    pub fn new(owner: impl Into<String>, bot: Arc<RuntimeBot>) -> Self {
        Self {
            owner: owner.into(),
            bot,
            commands: Vec::new(),
        }
    }

    pub fn register(mut self, command: Command) -> Self {
        self.commands.push(command);
        self
    }

    pub fn register_all(mut self, commands: impl IntoIterator<Item = Command>) -> Self {
        self.commands.extend(commands);
        self
    }

    pub fn install(self) -> Result<(), CommandRegistrationError> {
        let tree = Arc::new(CommandTree::new(self.commands)?);
        CommandCatalog::register(&self.owner, &tree)?;

        // kovi 禁用插件时先跑 drop 再 clear listen；目录不卸的话 /help 仍会列出已停用命令。
        let owner = self.owner.clone();
        plugin::drop(move || {
            let owner = owner.clone();
            async move {
                CommandCatalog::unregister(&owner);
            }
        });

        // kovi 把 on_msg 记在当前插件的 listen 上，禁用插件会 clear。
        // 分发必须跟命令所属插件同生共死，不能挂到第一个 install 的插件上。
        let bot = self.bot;
        bot::on_msg(move |event: Arc<dyn BotMsgEvent>| {
            let tree = Arc::clone(&tree);
            let bot = Arc::clone(&bot);
            async move {
                dispatch_msg(tree, bot, event).await;
            }
        });
        Ok(())
    }
}

async fn dispatch_msg(tree: Arc<CommandTree>, bot: Arc<RuntimeBot>, event: Arc<dyn BotMsgEvent>) {
    let Some(text) = extract_command_text(event.message()) else {
        return;
    };
    let resolved = match tree.resolve(&text) {
        ResolveOutcome::Ignored => return,
        ResolveOutcome::Error(error) => {
            if let Err(access_error) =
                check_event_access(event.as_ref(), &bot, error.scope(), error.permission())
            {
                reply_access_error(event.as_ref(), access_error);
                return;
            }
            event.reply(Message::from(error.to_string()));
            return;
        }
        ResolveOutcome::Matched(resolved) => resolved,
    };

    if let Err(error) = check_event_access(
        event.as_ref(),
        &bot,
        resolved.scope(),
        resolved.permission(),
    ) {
        reply_access_error(event.as_ref(), error);
        return;
    }

    let ResolvedCommand {
        path,
        args,
        rest,
        usage,
        handler,
        ..
    } = resolved;
    let context = CommandContext::new(
        Arc::clone(&event),
        Arc::clone(&bot),
        CommandArguments::new(args, rest),
    );
    let result = match handler {
        CommandHandler::Sync(handler) => handler(context),
        CommandHandler::Async(handler) => handler(context).await,
    };
    if let Err(error) = result {
        if let CommandError::Internal(internal) = &error {
            tracing::error!(
                command = %path.join(" "),
                user = %event.user_key(),
                group = ?event.group_key(),
                error = ?internal,
                "命令执行失败"
            );
        }
        event.reply(Message::from(render_command_error(&error, &usage)));
    }
}

pub(crate) fn extract_command_text(message: &Message) -> Option<String> {
    // 每条消息都会经过这里：单 text 段直接 clone，多段才拼合，
    // 避免为最常见的单段消息分配临时 Vec。
    let mut parts = message
        .iter()
        .filter(|segment| segment.kind == "text")
        .filter_map(|segment| segment.data.get("text").and_then(|value| value.as_str()));
    let first = parts.next()?;
    let mut combined = first.to_owned();
    for part in parts {
        combined.push('\n');
        combined.push_str(part);
    }
    Some(combined)
}

fn check_event_access(
    event: &dyn BotMsgEvent,
    bot: &RuntimeBot,
    scope: super::MessageScope,
    permission: Permission,
) -> Result<(), AccessError> {
    let source = if event.is_group() {
        MessageSource::Group
    } else {
        MessageSource::Private
    };
    let is_admin = permission == Permission::Everyone || user_is_bot_admin(bot, &event.user_key());
    check_access(scope, permission, source, is_admin)
}

fn reply_access_error(event: &dyn BotMsgEvent, error: AccessError) {
    event.reply(Message::from(error.to_string()));
}
