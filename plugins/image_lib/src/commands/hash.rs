use std::sync::Arc;

use utils::command::{
    Command, CommandContext, CommandError, CommandResult, MessageScope, Permission,
};

use crate::send::{OutgoingImage, image_message, report_send_fail, send_group_wait};
use crate::store::Store;

use super::{map_store_user_error, quoted_library_names};

pub(super) fn send_hash_command(store: Arc<Store>) -> Command {
    Command::new("哈希")
        .description("按内容哈希发送本群已存的图")
        .usage("哈希 <哈希或前缀>")
        .permission(Permission::BotAdmin)
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_send_hash(ctx, &store).await }
        })
}

pub(super) fn delete_hash_command(store: Arc<Store>) -> Command {
    Command::new("删除哈希")
        .description("按内容哈希删除本群已存的图")
        .usage("删除哈希 <哈希或前缀>")
        .permission(Permission::BotAdmin)
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_delete_hash(ctx, &store).await }
        })
}

fn parse_hash_prefix(raw: &str) -> Result<String, CommandError> {
    if raw.is_empty() {
        return Err(CommandError::MissingArgument {
            name: "哈希".to_owned(),
        });
    }
    if !(1..=64).contains(&raw.len()) || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(CommandError::user("哈希必须是 1 到 64 位十六进制"));
    }
    Ok(raw.to_ascii_lowercase())
}

async fn handle_send_hash(ctx: CommandContext, store: &Store) -> CommandResult {
    let prefix = parse_hash_prefix(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = ctx.group_id()?;
    let hash = match store.resolve_group_hash(group_id, &prefix).await {
        Ok(hash) => hash,
        Err(error) => return Err(map_store_user_error(error)),
    };
    let image = OutgoingImage::from_blob(store, group_id, &hash)
        .await
        .ok_or_else(|| CommandError::user("读取图片失败，请再试一次"))?;
    let message = image_message(None, &[image]);
    if let Err(error) = send_group_wait(ctx.bot(), group_id, &message).await {
        report_send_fail(
            ctx.bot(),
            store,
            group_id,
            format!("图库哈希发送失败 group={group_id}"),
            std::slice::from_ref(&hash),
            &error,
        )
        .await;
        return Err(CommandError::user("发送失败"));
    }
    Ok(())
}

async fn handle_delete_hash(ctx: CommandContext, store: &Store) -> CommandResult {
    let prefix = parse_hash_prefix(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = ctx.group_id()?;
    match store.delete_by_hash_prefix(group_id, &prefix).await {
        Ok(libraries) => {
            ctx.reply(format!(
                "已从{}删除这张图",
                quoted_library_names(&libraries)
            ));
            Ok(())
        }
        Err(error) => Err(map_store_user_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_prefix_accepts_hex_and_rejects_empty_or_junk() {
        assert_eq!(parse_hash_prefix("AbC").unwrap(), "abc");
        assert_eq!(parse_hash_prefix("a").unwrap(), "a");
        assert!(matches!(
            parse_hash_prefix(""),
            Err(CommandError::MissingArgument { .. })
        ));
        assert!(matches!(
            parse_hash_prefix("xyz"),
            Err(CommandError::User(_))
        ));
    }
}
