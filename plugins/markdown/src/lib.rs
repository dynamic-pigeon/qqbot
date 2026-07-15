use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};
use kovi_onebot::MessageRegistrar as _;
use utils::command::{Command, CommandContext, CommandError, CommandResult, CommandRouter};

const MAX_MARKDOWN_BYTES: usize = 32 * 1024;
const MARKDOWN_COOLDOWN: Duration = Duration::from_secs(10);
const COOLDOWN_ENTRY_TTL: Duration = Duration::from_secs(10 * 60);

static MARKDOWN_COOLDOWNS: LazyLock<Mutex<HashMap<i64, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn check_cooldown(user_id: i64) -> Option<u64> {
    let mut entries = MARKDOWN_COOLDOWNS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    entries.retain(|_, last| now.duration_since(*last) < COOLDOWN_ENTRY_TTL);
    if let Some(last) = entries.get(&user_id) {
        let elapsed = now.duration_since(*last);
        if elapsed < MARKDOWN_COOLDOWN {
            return Some((MARKDOWN_COOLDOWN - elapsed).as_secs().max(1));
        }
    }
    entries.insert(user_id, now);
    None
}

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
    if let Some(remaining) = check_cooldown(ctx.event().user_id) {
        return Err(CommandError::user(format!(
            "请求过于频繁，请在 {remaining} 秒后重试"
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

    #[test]
    fn markdown_command_resolves_without_content_for_error_handling() {
        let tree = CommandTree::new(vec![super::markdown_command()]).unwrap();
        assert!(matches!(tree.resolve("!md"), ResolveOutcome::Matched(_)));
    }
}
