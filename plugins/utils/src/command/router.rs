use std::sync::Arc;

use kovi::{PluginBuilder as plugin, RuntimeBot};
use kovi_onebot::EventRegistrar as _;

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
                let Some(text) = event.text.as_deref() else {
                    return;
                };
                let resolved = match tree.resolve(text) {
                    ResolveOutcome::Ignored => return,
                    ResolveOutcome::Error(error) => {
                        event.reply(error.to_string());
                        return;
                    }
                    ResolveOutcome::Matched(resolved) => resolved,
                };

                let (path, arguments, usage, permission, scope, handler) =
                    resolved.into_dispatch_parts();
                let source = if event.group_id.is_some() {
                    MessageSource::Group
                } else {
                    MessageSource::Private
                };
                let is_admin = permission == Permission::Everyone
                    || bot
                        .get_all_admin()
                        .unwrap_or_default()
                        .iter()
                        .any(|id| id.try_as_i64() == Some(event.user_id));
                if let Err(error) = check_access(scope, permission, source, is_admin) {
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

fn reply_access_error(event: &kovi_onebot::MsgEvent, error: AccessError) {
    event.reply(error.to_string());
}
