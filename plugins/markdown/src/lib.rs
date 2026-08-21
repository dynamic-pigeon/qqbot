use std::{sync::LazyLock, time::Duration};

use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};
use kovi_onebot::MessageRegistrar as _;
use utils::RateLimiter;
use utils::command::{Command, CommandContext, CommandError, CommandResult, CommandRouter};

const MAX_MARKDOWN_BYTES: usize = 32 * 1024;
const MARKDOWN_COOLDOWN: Duration = Duration::from_secs(10);

static MARKDOWN_COOLDOWNS: LazyLock<RateLimiter<i64>> =
    LazyLock::new(|| RateLimiter::new(MARKDOWN_COOLDOWN, 1));

fn markdown_command() -> Command {
    Command::new("!md")
        .description("根据 Markdown 生成图片")
        .usage("!md <Markdown 内容>")
        .handler(handle_markdown)
}

async fn handle_markdown(ctx: CommandContext) -> CommandResult {
    let md_content = ctx.rest();
    if ctx.trimmed_rest().is_empty() {
        return Err(CommandError::MissingArgument {
            name: "Markdown 内容".to_owned(),
        });
    }
    if md_content.len() > MAX_MARKDOWN_BYTES {
        return Err(CommandError::user(format!(
            "Markdown 内容过长，最大允许 {} KiB",
            MAX_MARKDOWN_BYTES / 1024
        )));
    }
    if let Err(hit) = MARKDOWN_COOLDOWNS.try_acquire(ctx.event().user_id) {
        return Err(CommandError::user(format!(
            "请求过于频繁，请在 {} 秒后重试",
            hit.retry_after_secs()
        )));
    }

    let img = utils::md_to_img(md_content)
        .await
        .map_err(CommandError::internal)?;
    let base64_img = base64::engine::general_purpose::STANDARD.encode(&img);
    let message = Message::new().add_image(&format!("base64://{base64_img}"));
    ctx.reply_and_quote(message);
    Ok(())
}

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    CommandRouter::new("markdown", bot)
        .register(markdown_command())
        .install()
        .expect("注册 !md 命令失败");
}

#[cfg(test)]
mod tests {
    use utils::command::{CommandTree, ResolveOutcome};

    #[test]
    fn markdown_command_preserves_raw_content() {
        let tree = CommandTree::new(vec![super::markdown_command()]).unwrap();
        let ResolveOutcome::Matched(command) = tree.resolve("!md   # title") else {
            panic!("expected !md to resolve");
        };

        assert_eq!(command.path(), ["!md"]);
        assert_eq!(command.rest(), "  # title");
    }
}
