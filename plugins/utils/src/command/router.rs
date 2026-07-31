use std::sync::Arc;

use kovi::{Message, PluginBuilder as plugin, RuntimeBot};
use kovi_onebot::{EventRegistrar as _, MsgEvent};

use super::{
    AccessError, Command, CommandCatalog, CommandContext, CommandError, CommandRegistrationError,
    CommandTree, MessageSource, Permission, ResolveOutcome, check_access, render_command_error,
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

    pub fn install(self) -> Result<(), CommandRegistrationError> {
        let tree = Arc::new(CommandTree::new(self.commands)?);
        CommandCatalog::register(&self.owner, &tree)?;

        let bot = self.bot;
        plugin::on_msg(move |event| {
            let tree = Arc::clone(&tree);
            let bot = Arc::clone(&bot);
            async move {
                let Some(text) = extract_command_text(&event.message) else {
                    return;
                };
                let resolved = match tree.resolve(&text) {
                    ResolveOutcome::Ignored => return,
                    ResolveOutcome::Error(error) => {
                        if let Err(access_error) =
                            check_event_access(&event, &bot, error.scope(), error.permission())
                        {
                            reply_access_error(&event, access_error);
                            return;
                        }
                        event.reply(error.to_string());
                        return;
                    }
                    ResolveOutcome::Matched(resolved) => resolved,
                };

                let (path, arguments, usage, permission, scope, handler) =
                    resolved.into_dispatch_parts();
                if let Err(error) = check_event_access(&event, &bot, scope, permission) {
                    reply_access_error(&event, error);
                    return;
                }

                let context = CommandContext::new(
                    Arc::clone(&event),
                    Arc::clone(&bot),
                    path.clone(),
                    arguments,
                );
                if let Err(error) = handler(context).await {
                    if let CommandError::Internal(internal) = &error {
                        tracing::error!(
                            command = %path.join(" "),
                            user_id = event.user_id,
                            group_id = event.group_id,
                            error = ?internal,
                            "命令执行失败"
                        );
                    }
                    event.reply(render_command_error(&error, &usage));
                }
            }
        });
        Ok(())
    }
}

pub fn extract_command_text(message: &Message) -> Option<String> {
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
    event: &MsgEvent,
    bot: &RuntimeBot,
    scope: super::MessageScope,
    permission: Permission,
) -> Result<(), AccessError> {
    // 群临时会话的私聊消息同样携带 group_id，只有 message_type 能可靠区分来源。
    let source = if event.message_type == "group" {
        MessageSource::Group
    } else {
        MessageSource::Private
    };
    check_access(scope, Permission::Everyone, source, false)?;

    let is_admin = permission == Permission::Everyone
        || bot
            .get_all_admin()
            .unwrap_or_default()
            .iter()
            .any(|id| id.try_as_i64() == Some(event.user_id));
    check_access(super::MessageScope::Any, permission, source, is_admin)
}

fn reply_access_error(event: &MsgEvent, error: AccessError) {
    event.reply(error.to_string());
}
