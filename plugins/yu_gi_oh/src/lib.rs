use std::{sync::LazyLock, time::Duration};

use base64::Engine as _;
use kovi::{Message, PluginBuilder as plugin};
use kovi_onebot::MessageRegistrar as _;
use utils::RateLimiter;
use utils::command::{Command, CommandContext, CommandError, CommandResult, CommandRouter};

mod fetch_card;

const MAX_CARD_NAME_BYTES: usize = 128;
const QUERY_COOLDOWN: Duration = Duration::from_secs(3);

static QUERY_LIMITER: LazyLock<RateLimiter<i64>> =
    LazyLock::new(|| RateLimiter::new(QUERY_COOLDOWN, 1));

fn card_query_command() -> Command {
    Command::new("/查卡")
        .description("查询游戏王卡片信息")
        .usage("/查卡 <卡片名称>")
        .handler(handle_card_query)
}

async fn handle_card_query(ctx: CommandContext) -> CommandResult {
    let card_name = ctx.trimmed_rest();
    if card_name.is_empty() {
        return Err(CommandError::MissingArgument {
            name: "卡片名称".to_owned(),
        });
    }
    if card_name.len() > MAX_CARD_NAME_BYTES {
        return Err(CommandError::user("卡片名称过长"));
    }
    if QUERY_LIMITER.try_acquire(ctx.event().user_id).is_err() {
        return Err(CommandError::user("查询过于频繁，请稍后重试"));
    }

    let Some(card) = fetch_card::fetch_card(card_name)
        .await
        .map_err(CommandError::internal)?
    else {
        return Err(CommandError::user(format!("未找到卡片：{card_name}")));
    };
    let img = match card.fetch_image().await {
        Ok(data) => data,
        Err(error) => {
            tracing::error!("Fetch card image error: {error}");
            ctx.reply("获取卡片图片失败");
            ctx.reply(format!("{card}"));
            return Ok(());
        }
    };

    let base64_img = base64::engine::general_purpose::STANDARD.encode(img);
    let message = Message::new()
        .add_image(&format!("base64://{base64_img}"))
        .add_text(format!("{card}"));
    ctx.reply(message);
    Ok(())
}

#[kovi::plugin]
async fn main() {
    let bot = plugin::get_runtime_bot();
    CommandRouter::new("yu_gi_oh", bot)
        .register(card_query_command())
        .install()
        .expect("注册 /查卡 命令失败");
}

#[cfg(test)]
mod tests {
    use utils::command::{CommandTree, ResolveOutcome};

    #[test]
    fn card_query_uses_trimmed_remaining_text() {
        let tree = CommandTree::new(vec![super::card_query_command()]).unwrap();
        let ResolveOutcome::Matched(command) = tree.resolve("/查卡   青眼 白龙  ") else {
            panic!("expected /查卡 to resolve");
        };

        assert_eq!(command.path(), ["/查卡"]);
        assert_eq!(command.trimmed_rest(), "青眼 白龙");
    }
}
