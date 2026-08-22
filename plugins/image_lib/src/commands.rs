use std::sync::Arc;

use base64::Engine as _;
use kovi::{Message, Segment};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};
use utils::RateLimiter;
use utils::command::{Command, CommandContext, CommandError, CommandResult, MessageScope};

use crate::fetch::{
    FetchError, MAX_ADD_IMAGES, extract_reply_id, load_image_bytes, parse_message_segments,
    select_images,
};
use crate::name::parse_library_name;
use crate::store::{Store, StoreError, sha256_hex};

pub fn image_lib_command(store: Arc<Store>, limiter: Arc<RateLimiter<i64>>) -> Command {
    Command::new("图库")
        .description("管理本群图库")
        .usage("图库")
        .scope(MessageScope::Group)
        .handler({
            let store = Arc::clone(&store);
            move |ctx| {
                let store = Arc::clone(&store);
                async move { handle_list(ctx, &store).await }
            }
        })
        .subcommand(add_command(Arc::clone(&store)))
        .subcommand(draw_command(Arc::clone(&store), limiter))
        .subcommand(delete_command(Arc::clone(&store)))
        .subcommand(alias_command(Arc::clone(&store)))
        .subcommand(unalias_command(store))
}

fn add_command(store: Arc<Store>) -> Command {
    Command::new("添加")
        .description("回复一张或多张图，写入本群指定图库")
        .usage("添加 <库名>")
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_add(ctx, &store).await }
        })
}

fn draw_command(store: Arc<Store>, limiter: Arc<RateLimiter<i64>>) -> Command {
    Command::new("来只")
        .description("从本群指定图库随机发一张图")
        .usage("来只 <库名>")
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let limiter = Arc::clone(&limiter);
            async move { handle_draw(ctx, &store, &limiter).await }
        })
}

fn delete_command(store: Arc<Store>) -> Command {
    Command::new("删除")
        .description("回复一张图删除该图；管理员删除库名或别名则清空整个库")
        .usage("删除\n删除 <库名或别名>")
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_delete(ctx, &store).await }
        })
}

fn alias_command(store: Arc<Store>) -> Command {
    Command::new("别名")
        .description("给已有图库起别名，来只/添加/删除都走同一库")
        .usage("别名 <别名> <库名>")
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_alias(ctx, &store).await }
        })
}

fn unalias_command(store: Arc<Store>) -> Command {
    Command::new("取消别名")
        .description("去掉一个图库别名，不删除图片")
        .usage("取消别名 <别名>")
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_unalias(ctx, &store).await }
        })
}

async fn handle_add(ctx: CommandContext, store: &Store) -> CommandResult {
    let name = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = group_id(&ctx)?;
    let reply_id = extract_reply_id(&ctx.event().message)
        .ok_or_else(|| CommandError::user("请回复一张包含图片的消息后再添加"))?;

    let segments = replied_image_segments(&ctx, reply_id, MAX_ADD_IMAGES, false).await?;
    let mut images = Vec::with_capacity(segments.len());
    for segment in &segments {
        let bytes = match load_image_bytes(segment).await {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = store.discard_unindexed(group_id, &images).await;
                return Err(error.into());
            }
        };
        match store.write_blob(group_id, bytes).await {
            Ok(image) => images.push(image),
            Err(error) => {
                let _ = store.discard_unindexed(group_id, &images).await;
                return Err(CommandError::internal(error));
            }
        }
    }
    match store.add_images(group_id, name, images).await {
        Ok(result) if result.added == 0 => {
            ctx.reply(format!("都已在「{name}」里"));
            Ok(())
        }
        Ok(result) => {
            ctx.reply(format!("已向「{name}」添加 {} 张", result.added));
            Ok(())
        }
        Err(error) => Err(map_store_user_error(error)),
    }
}

async fn handle_alias(ctx: CommandContext, store: &Store) -> CommandResult {
    let alias = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    let target = parse_library_name(ctx.arg(1).unwrap_or(""))?;
    ctx.ensure_no_extra_args(2)?;
    let group_id = group_id(&ctx)?;
    match store.set_alias(group_id, alias, target).await {
        Ok(canonical) => {
            ctx.reply(format!("「{alias}」现在是「{canonical}」的别名"));
            Ok(())
        }
        Err(error) => Err(map_store_user_error(error)),
    }
}

async fn handle_unalias(ctx: CommandContext, store: &Store) -> CommandResult {
    let alias = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = group_id(&ctx)?;
    match store.remove_alias(group_id, alias).await {
        Ok(()) => {
            ctx.reply(format!("已取消别名「{alias}」"));
            Ok(())
        }
        Err(error) => Err(map_store_user_error(error)),
    }
}

async fn handle_draw(
    ctx: CommandContext,
    store: &Store,
    limiter: &RateLimiter<i64>,
) -> CommandResult {
    let name = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = group_id(&ctx)?;

    let hash = match store.pick_random(group_id, name).await {
        Ok(hash) => hash,
        Err(StoreError::LibraryMissing | StoreError::LibraryEmpty) => {
            ctx.reply(format!("「{name}」里还没有图"));
            return Ok(());
        }
        Err(error) => return Err(CommandError::internal(error)),
    };

    // 空库已经返回。先 peek 限流再读盘，避免打满后还读整张图。
    if let Err(hit) = limiter.check(&group_id) {
        return Err(rate_limited(hit));
    }

    tracing::debug!("图库 draw group_id={} hash={}", group_id, hash);

    let bytes = store
        .read_blob(group_id, &hash)
        .await
        .map_err(|_| CommandError::user("读取图片失败，请再试一次"))?;
    if let Err(hit) = limiter.try_acquire(group_id) {
        return Err(rate_limited(hit));
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    ctx.reply(Message::new().add_image(&format!("base64://{encoded}")));
    Ok(())
}

async fn handle_delete(ctx: CommandContext, store: &Store) -> CommandResult {
    ctx.ensure_no_extra_args(1)?;
    let group_id = group_id(&ctx)?;
    match ctx.arg(0) {
        None => delete_one_image(&ctx, store, group_id).await,
        Some(name) => {
            if !is_bot_admin(&ctx) {
                return Err(CommandError::user("管理员专用命令，普通用户无法使用"));
            }
            let name = parse_library_name(name)?;
            match store.wipe_library(group_id, name).await {
                Ok(canonical) => {
                    ctx.reply(format!("已清空「{canonical}」"));
                    Ok(())
                }
                Err(StoreError::LibraryMissing) => {
                    Err(CommandError::user(format!("「{name}」不存在")))
                }
                Err(error) => Err(map_store_user_error(error)),
            }
        }
    }
}

async fn delete_one_image(ctx: &CommandContext, store: &Store, group_id: i64) -> CommandResult {
    let reply_id = extract_reply_id(&ctx.event().message)
        .ok_or_else(|| CommandError::user("请回复一张图后再发送「删除」"))?;
    let images = load_replied_images(ctx, reply_id, 1, true).await?;
    let hash = sha256_hex(&images[0]);
    match store.delete_hash(group_id, &hash).await {
        Ok(libraries) => {
            let names = libraries
                .iter()
                .map(|name| format!("「{name}」"))
                .collect::<Vec<_>>()
                .join("");
            ctx.reply(format!("已从{names}删除这张图"));
            Ok(())
        }
        Err(StoreError::ImageMissing) => Err(CommandError::user("没有这张图")),
        Err(error) => Err(CommandError::internal(error)),
    }
}

async fn handle_list(ctx: CommandContext, store: &Store) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = group_id(&ctx)?;
    let stats = store
        .stats(group_id)
        .await
        .map_err(CommandError::internal)?;
    if stats.libraries.is_empty() {
        ctx.reply("本群还没有图库");
        return Ok(());
    }

    let mut lines = vec![format!(
        "本群图库（{} 个，共 {} 张，占用 {} / {}）",
        stats.libraries.len(),
        stats.unique_count,
        format_bytes(stats.unique_bytes),
        format_bytes(store.max_group_bytes())
    )];
    for library in stats.libraries {
        let alias_note = if library.aliases.is_empty() {
            String::new()
        } else {
            format!("（{}）", library.aliases.join("、"))
        };
        lines.push(format!(
            "• {}{alias_note}: {} 张，{}",
            library.name,
            library.count,
            format_bytes(library.bytes)
        ));
    }
    ctx.reply(lines.join("\n"));
    Ok(())
}

async fn replied_image_segments(
    ctx: &CommandContext,
    reply_id: i32,
    max: usize,
    single: bool,
) -> Result<Vec<Segment>, CommandError> {
    let response = ctx
        .bot()
        .get_msg(reply_id)
        .await
        .map_err(|_| FetchError::MessageUnavailable)?;
    if response.status != "ok" {
        return Err(FetchError::MessageUnavailable.into());
    }
    let segments =
        parse_message_segments(&response.data).map_err(|_| FetchError::MessageUnavailable)?;
    let images = select_images(&segments, max, single)?;
    Ok(images.into_iter().cloned().collect())
}

async fn load_replied_images(
    ctx: &CommandContext,
    reply_id: i32,
    max: usize,
    single: bool,
) -> Result<Vec<Vec<u8>>, CommandError> {
    let segments = replied_image_segments(ctx, reply_id, max, single).await?;
    let mut loaded = Vec::with_capacity(segments.len());
    for segment in &segments {
        loaded.push(load_image_bytes(segment).await?);
    }
    Ok(loaded)
}

fn map_store_user_error(error: StoreError) -> CommandError {
    match error {
        StoreError::QuotaExceeded { used, limit, .. } => CommandError::user(format!(
            "本群图库容量不足（已用 {} / {}）",
            format_bytes(used),
            format_bytes(limit)
        )),
        StoreError::LibraryMissing => CommandError::user("库不存在"),
        StoreError::ImageMissing => CommandError::user("没有这张图"),
        StoreError::AliasToSelf
        | StoreError::NameIsLibrary(_)
        | StoreError::TargetMissing(_)
        | StoreError::AliasMissing(_) => CommandError::user(error.to_string()),
        other => CommandError::internal(other),
    }
}

fn rate_limited(hit: utils::RateLimitHit) -> CommandError {
    CommandError::user(format!("请在 {} 秒后再试", hit.retry_after_secs()))
}

fn group_id(ctx: &CommandContext) -> Result<i64, CommandError> {
    ctx.event()
        .group_id
        .ok_or_else(|| CommandError::user("此命令只能在群聊中使用"))
}

fn is_bot_admin(ctx: &CommandContext) -> bool {
    ctx.bot()
        .get_all_admin()
        .unwrap_or_default()
        .iter()
        .any(|id| id.try_as_i64() == Some(ctx.event().user_id))
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB)
    }
}
