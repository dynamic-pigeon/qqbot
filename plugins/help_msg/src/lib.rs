use kovi::PluginBuilder as plugin;
use utils::command::{Command, CommandCatalog, CommandRouter};

fn help_command() -> Command {
    Command::new("/help")
        .description("查看可用命令和具体用法")
        .usage("/help [命令路径]")
        .handler(|ctx| async move {
            let path = ctx.args().iter().map(String::as_str).collect::<Vec<_>>();
            ctx.reply(CommandCatalog::render_help(&path));
            Ok(())
        })
}

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    CommandRouter::new("help_msg", bot)
        .register(help_command())
        .install()
        .expect("注册 /help 命令失败");
}

#[cfg(test)]
mod tests {
    use utils::command::{CommandTree, ResolveOutcome};

    #[test]
    fn help_command_resolves_nested_help_paths_as_arguments() {
        let tree = CommandTree::new(vec![super::help_command()]).unwrap();
        let ResolveOutcome::Matched(command) = tree.resolve("/help live add") else {
            panic!("expected /help to resolve");
        };

        assert_eq!(command.path(), ["/help"]);
        assert_eq!(command.args(), ["live", "add"]);
    }
}
