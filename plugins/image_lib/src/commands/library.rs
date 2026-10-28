use std::sync::Arc;

use kovi::Segment;
use kovi_onebot::OnebotTrait;
use utils::command::{
    Command, CommandContext, CommandError, CommandResult, MessageScope, Permission,
};
use utils::{RateLimiter, sha256_hex};

use crate::confirm::WipeConfirmations;
use crate::fetch::{
    AddImageSource, FetchError, MAX_ADD_IMAGES, load_image_bytes, parse_message_segments,
    resolve_add_source, select_images, stage_image,
};
use crate::name::parse_library_name;
use crate::send::{OutgoingImage, image_message, report_send_fail, send_group_wait};
use crate::store::{StagedImage, Store, StoreError};

use super::{
    confirm_key, format_bytes, map_store_user_error, missing_library, quoted_library_names,
};

pub(super) fn list_command(store: Arc<Store>, wipes: Arc<WipeConfirmations>) -> Command {
    Command::new("图库")
        .description("管理本群图库")
        .usage("图库")
        .scope(MessageScope::Group)
        .group("图库")
        .handler({
            let store = Arc::clone(&store);
            move |ctx| {
                let store = Arc::clone(&store);
                async move { handle_list(ctx, &store).await }
            }
        })
        .subcommand(confirm_command(Arc::clone(&store), Arc::clone(&wipes)))
        .subcommand(cancel_command(wipes))
}

pub(super) fn add_command(store: Arc<Store>) -> Command {
    Command::new("添加")
        .description("把本条消息或回复里的图写入本群指定图库")
        .usage("添加 <库名>")
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_add(ctx, &store).await }
        })
}

pub(super) fn draw_command(store: Arc<Store>, limiter: Arc<RateLimiter<i64>>) -> Command {
    Command::new("来只")
        .description("从本群指定图库随机发一张图")
        .usage("来只 <库名>")
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let limiter = Arc::clone(&limiter);
            async move { handle_draw(ctx, &store, &limiter).await }
        })
}

pub(super) fn delete_command(store: Arc<Store>, wipes: Arc<WipeConfirmations>) -> Command {
    Command::new("删除")
        .description(
            "带图或回复一张图删除该图；管理员删除库名或别名会登记待清空，需「图库 确认」执行",
        )
        .usage("删除\n删除 <库名或别名>")
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let wipes = Arc::clone(&wipes);
            async move { handle_delete(ctx, &store, &wipes).await }
        })
}

/// 挂在「图库」下而不提为根命令：确认入口必须走完整的「图库 确认」，
/// 避免群里随口说「确认」误触发。
fn confirm_command(store: Arc<Store>, wipes: Arc<WipeConfirmations>) -> Command {
    Command::new("确认")
        .description("确认执行登记的清空图库操作")
        .usage("图库 确认")
        .scope(MessageScope::Group)
        .permission(Permission::BotAdmin)
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let wipes = Arc::clone(&wipes);
            async move { handle_confirm(ctx, &store, &wipes).await }
        })
}

fn cancel_command(wipes: Arc<WipeConfirmations>) -> Command {
    Command::new("取消")
        .description("撤销登记的清空图库操作")
        .usage("图库 取消")
        .scope(MessageScope::Group)
        .permission(Permission::BotAdmin)
        .handler(move |ctx| {
            let wipes = Arc::clone(&wipes);
            async move { handle_cancel(ctx, &wipes).await }
        })
}

pub(super) fn alias_command(store: Arc<Store>) -> Command {
    Command::new("别名")
        .description("给已有图库起别名；末尾加「合并」会把原库并进目标库")
        .usage("别名 <别名> <库名> [合并]")
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_alias(ctx, &store).await }
        })
}

pub(super) fn unalias_command(store: Arc<Store>) -> Command {
    Command::new("取消别名")
        .description("去掉一个图库别名，不删除图片")
        .usage("取消别名 <别名>")
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_unalias(ctx, &store).await }
        })
}

async fn handle_add(ctx: CommandContext, store: &Store) -> CommandResult {
    let name = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = ctx.group_id()?;
    let segments = add_image_segments(&ctx).await?;
    let staged = stage_all(store, group_id, &segments).await?;
    match store.add_images(group_id, name, staged).await {
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

/// 逐张把消息里的图片流式下载到 blobs 目录旁并计算内容哈希。
/// 中途失败时清理已落盘的临时文件，不留半成品。
async fn stage_all(
    store: &Store,
    group_id: i64,
    segments: &[Segment],
) -> Result<Vec<StagedImage>, CommandError> {
    let blobs = store.blobs_dir(group_id);
    let mut staged: Vec<StagedImage> = Vec::with_capacity(segments.len());
    for segment in segments {
        let file = match stage_image(segment, &blobs).await {
            Ok(file) => file,
            Err(error) => {
                for image in &staged {
                    let _ = kovi::tokio::fs::remove_file(&image.path).await;
                }
                return Err(error.into());
            }
        };
        let hash = utils::sha256_hex_file(&file.path)
            .await
            .map_err(|_| FetchError::Unreadable)?;
        staged.push(StagedImage {
            hash,
            size: file.size,
            path: file.path,
        });
    }
    Ok(staged)
}

async fn handle_alias(ctx: CommandContext, store: &Store) -> CommandResult {
    let alias = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    let target = parse_library_name(ctx.arg(1).unwrap_or(""))?;
    let merge = match ctx.arg(2) {
        None => false,
        Some("合并") => true,
        Some(_) => return Err(CommandError::UnexpectedArgument),
    };
    ctx.ensure_no_extra_args(if merge { 3 } else { 2 })?;
    let group_id = ctx.group_id()?;
    match store.set_alias(group_id, alias, target, merge).await {
        Ok(result) => {
            let canonical = &result.canonical;
            let message = if let Some(from) = &result.merged_from {
                format!(
                    "已将「{from}」合并进「{canonical}」，「{alias}」现在是「{canonical}」的别名"
                )
            } else {
                format!("「{alias}」现在是「{canonical}」的别名")
            };
            ctx.reply(message);
            Ok(())
        }
        Err(error) => Err(map_store_user_error(error)),
    }
}

async fn handle_unalias(ctx: CommandContext, store: &Store) -> CommandResult {
    let alias = parse_library_name(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = ctx.group_id()?;
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
    let group_id = ctx.group_id()?;

    match store.resolve_name(group_id, name).await {
        Ok(_) => {}
        Err(StoreError::LibraryMissing) => {
            ctx.reply(format!("「{name}」里还没有图"));
            return Ok(());
        }
        Err(error) => return Err(CommandError::internal(error)),
    }

    // 空库不占限流。先占名额再抽并计数，打满的请求不会把次数加上。
    if let Err(hit) = limiter.try_acquire(group_id) {
        return Err(rate_limited(hit));
    }

    let hash = match store.pick_random(group_id, name).await {
        Ok(hash) => hash,
        Err(StoreError::LibraryMissing | StoreError::LibraryEmpty) => {
            ctx.reply(format!("「{name}」里还没有图"));
            return Ok(());
        }
        Err(error) => return Err(CommandError::internal(error)),
    };

    tracing::debug!("图库 draw group_id={} hash={}", group_id, hash);

    let image = OutgoingImage::from_blob(store, group_id, &hash)
        .await
        .ok_or_else(|| CommandError::user("读取图片失败，请再试一次"))?;

    let message = image_message(None, &[image]);
    if let Err(error) = send_group_wait(ctx.bot(), group_id, &message).await {
        report_send_fail(
            ctx.bot(),
            store,
            group_id,
            format!("图库来只发送失败 group={group_id} 库={name}"),
            std::slice::from_ref(&hash),
            &error,
        )
        .await;
        return Err(CommandError::user("发送失败"));
    }
    Ok(())
}

async fn handle_delete(
    ctx: CommandContext,
    store: &Store,
    wipes: &WipeConfirmations,
) -> CommandResult {
    ctx.ensure_no_extra_args(1)?;
    let group_id = ctx.group_id()?;
    match ctx.arg(0) {
        None => delete_one_image(&ctx, store, group_id).await,
        Some(name) => {
            ctx.require_admin()?;
            let name = parse_library_name(name)?;
            match store.library_overview(group_id, name).await {
                Ok(overview) => {
                    wipes.begin(confirm_key(&ctx, group_id), overview.canonical.clone());
                    ctx.reply(format!(
                        "「{}」有 {} 张图（约 {}）。确认清空请发送「图库 确认」，发送「图库 取消」可撤销，{} 分钟内有效",
                        overview.canonical,
                        overview.count,
                        format_bytes(overview.bytes),
                        wipes.pending_ttl_minutes(),
                    ));
                    Ok(())
                }
                Err(error) => Err(missing_library(name, error)),
            }
        }
    }
}

async fn handle_confirm(
    ctx: CommandContext,
    store: &Store,
    wipes: &WipeConfirmations,
) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    let Some(library) = wipes.take(&confirm_key(&ctx, group_id)) else {
        return Err(CommandError::user(
            "没有待确认的清空操作，或已超时。可先发送「删除 <库名>」",
        ));
    };
    match store.wipe_canonical_library(group_id, &library).await {
        Ok(canonical) => {
            ctx.reply(format!("已清空「{canonical}」"));
            Ok(())
        }
        Err(StoreError::LibraryMissing) => Err(CommandError::user(format!(
            "「{library}」已不存在（可能已被清空或改名），请重新发送「删除 <库名>」"
        ))),
        Err(error) => Err(map_store_user_error(error)),
    }
}

async fn handle_cancel(ctx: CommandContext, wipes: &WipeConfirmations) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
    match wipes.take(&confirm_key(&ctx, group_id)) {
        Some(library) => {
            ctx.reply(format!("已取消清空「{library}」"));
            Ok(())
        }
        None => Err(CommandError::user("没有待取消的清空操作")),
    }
}

async fn delete_one_image(ctx: &CommandContext, store: &Store, group_id: i64) -> CommandResult {
    let segments = collect_image_segments(
        ctx,
        1,
        true,
        "请在这条消息里带图，或回复一张包含图片的消息后再删除",
    )
    .await?;
    let bytes = load_replied_bytes(&segments).await?;
    let hash = sha256_hex(&bytes[0]);
    match store.delete_hash(group_id, &hash).await {
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

async fn handle_list(ctx: CommandContext, store: &Store) -> CommandResult {
    ctx.ensure_no_extra_args(0)?;
    let group_id = ctx.group_id()?;
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

async fn add_image_segments(ctx: &CommandContext) -> Result<Vec<Segment>, CommandError> {
    collect_image_segments(
        ctx,
        MAX_ADD_IMAGES,
        false,
        "请在这条消息里带图，或回复一张包含图片的消息后再添加",
    )
    .await
}

/// 本条消息带图优先，否则用回复消息里的图；「添加」与「删除」共用。
async fn collect_image_segments(
    ctx: &CommandContext,
    max: usize,
    single: bool,
    missing_hint: &str,
) -> Result<Vec<Segment>, CommandError> {
    let message = &ctx.event().message;
    match resolve_add_source(message) {
        AddImageSource::Current => {
            let images = select_images(message.iter(), max, single)?;
            Ok(images.into_iter().cloned().collect())
        }
        AddImageSource::Reply(reply_id) => replied_image_segments(ctx, reply_id, max, single).await,
        AddImageSource::Missing => Err(CommandError::user(missing_hint)),
    }
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

async fn load_replied_bytes(segments: &[Segment]) -> Result<Vec<Vec<u8>>, CommandError> {
    let mut loaded = Vec::with_capacity(segments.len());
    for segment in segments {
        loaded.push(load_image_bytes(segment).await?);
    }
    Ok(loaded)
}

fn rate_limited(hit: utils::RateLimitHit) -> CommandError {
    CommandError::user(format!("请在 {} 秒后再试", hit.retry_after_secs()))
}
