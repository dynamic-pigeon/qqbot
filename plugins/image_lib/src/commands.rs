use std::sync::Arc;

use base64::Engine as _;
use kovi::{Message, Segment};
use kovi_onebot::{MessageRegistrar as _, OnebotTrait};
use utils::RateLimiter;
use utils::command::{
    Command, CommandContext, CommandError, CommandResult, MessageScope, Permission,
};

use crate::fetch::{
    FetchError, MAX_ADD_IMAGES, extract_reply_id, load_image_bytes, parse_message_segments,
    select_images,
};
use crate::name::parse_library_name;
use crate::scan::{
    NEXT_PAGE_ARG, ScanAdvance, ScanKey, ScanSessions, group_title, packetize_images,
    parse_group_index,
};
use crate::similar::{cluster, distance_from_percent};
use crate::store::{Store, StoreError, sha256_hex};

pub fn image_lib_command(store: Arc<Store>, limiter: Arc<RateLimiter<i64>>) -> Command {
    let scans = Arc::new(ScanSessions::new());
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
        .subcommand(unalias_command(Arc::clone(&store)))
        .subcommand(send_hash_command(Arc::clone(&store)))
        .subcommand(delete_hash_command(Arc::clone(&store)))
        .subcommand(scan_command(store, scans))
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

fn send_hash_command(store: Arc<Store>) -> Command {
    Command::new("哈希")
        .description("按内容哈希发送本群已存的图")
        .usage("哈希 <哈希或前缀>")
        .permission(Permission::BotAdmin)
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_send_hash(ctx, &store).await }
        })
}

fn delete_hash_command(store: Arc<Store>) -> Command {
    Command::new("删除哈希")
        .description("按内容哈希删除本群已存的图")
        .usage("删除哈希 <哈希或前缀>")
        .permission(Permission::BotAdmin)
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            async move { handle_delete_hash(ctx, &store).await }
        })
}

fn scan_command(store: Arc<Store>, scans: Arc<ScanSessions>) -> Command {
    Command::new("查重")
        .description("扫指定图库的近重复，不删除")
        .usage("查重 <库名> [组号|下一组|相似度%]")
        .permission(Permission::BotAdmin)
        .expose_as_root()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let scans = Arc::clone(&scans);
            async move { handle_scan(ctx, &store, &scans).await }
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
    let group_id = group_id(&ctx)?;
    let bytes = match store.load_by_hash_prefix(group_id, &prefix).await {
        Ok(bytes) => bytes,
        Err(error) => return Err(map_store_user_error(error)),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    ctx.reply(Message::new().add_image(&format!("base64://{encoded}")));
    Ok(())
}

async fn handle_delete_hash(ctx: CommandContext, store: &Store) -> CommandResult {
    let prefix = parse_hash_prefix(ctx.arg(0).unwrap_or(""))?;
    ctx.ensure_no_extra_args(1)?;
    let group_id = group_id(&ctx)?;
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
            ctx.reply(format!(
                "已从{}删除这张图",
                quoted_library_names(&libraries)
            ));
            Ok(())
        }
        Err(StoreError::ImageMissing) => Err(CommandError::user("没有这张图")),
        Err(error) => Err(CommandError::internal(error)),
    }
}

enum ScanOp<'a> {
    Start { name: &'a str, percent: Option<u32> },
    Next { name: &'a str },
    Jump { name: &'a str, index: usize },
}

fn parse_percent_arg(raw: &str) -> Result<u32, CommandError> {
    let number = raw
        .strip_suffix('%')
        .or_else(|| raw.strip_suffix('％'))
        .ok_or_else(|| CommandError::InvalidArgument {
            name: "相似度%".to_owned(),
        })?;
    let parsed: u32 = number.parse().map_err(|_| CommandError::InvalidArgument {
        name: "相似度%".to_owned(),
    })?;
    if (1..=100).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(CommandError::user("相似度% 需要是 1 到 100 的整数"))
    }
}

fn parse_scan_op(args: &[String]) -> Result<ScanOp<'_>, CommandError> {
    let name = parse_library_name(args.first().map(String::as_str).unwrap_or(""))?;
    let extra = args.get(1).map(String::as_str);
    if extra.is_some() && args.len() > 2 {
        return Err(CommandError::UnexpectedArgument);
    }
    match extra {
        None => Ok(ScanOp::Start {
            name,
            percent: None,
        }),
        Some(arg) if arg == NEXT_PAGE_ARG => Ok(ScanOp::Next { name }),
        Some(arg) if arg.bytes().all(|byte| byte.is_ascii_digit()) => {
            let index =
                parse_group_index(arg).ok_or_else(|| CommandError::user("组号从 1 开始"))?;
            Ok(ScanOp::Jump { name, index })
        }
        Some(arg) => Ok(ScanOp::Start {
            name,
            percent: Some(
                parse_percent_arg(arg)
                    .map_err(|_| CommandError::user("第二参数是组号、下一组或相似度（如 90%）"))?,
            ),
        }),
    }
}

fn missing_library(name: &str, error: StoreError) -> CommandError {
    match error {
        StoreError::LibraryMissing => CommandError::user(format!("「{name}」不存在")),
        other => CommandError::internal(other),
    }
}

async fn handle_scan(ctx: CommandContext, store: &Store, scans: &ScanSessions) -> CommandResult {
    let op = parse_scan_op(ctx.args())?;
    let group_id = group_id(&ctx)?;
    let user_id = ctx.event().user_id;
    match op {
        ScanOp::Start { name, percent } => {
            let (canonical, images) = store
                .fingerprints_for_library(group_id, name)
                .await
                .map_err(|error| missing_library(name, error))?;
            if images.len() < 2 {
                ctx.reply(format!("「{canonical}」里没有相似的图"));
                return Ok(());
            }
            let config = crate::config::static_config();
            let duplicate = percent
                .map(distance_from_percent)
                .unwrap_or_else(|| config.duplicate_distance());
            let groups = cluster(&images, duplicate, config.maybe_distance());
            if groups.is_empty() {
                ctx.reply(format!("「{canonical}」里没有相似的图"));
                return Ok(());
            }
            let key = ScanKey {
                group_id,
                user_id,
                library: canonical.clone(),
            };
            scans.start(key.clone(), groups);
            show_scan_group(&ctx, store, scans, group_id, &canonical, &key, None, true).await
        }
        ScanOp::Next { name } => {
            let (canonical, key) = open_scan_key(store, group_id, user_id, name).await?;
            show_scan_group(&ctx, store, scans, group_id, &canonical, &key, None, false).await
        }
        ScanOp::Jump { name, index } => {
            let (canonical, key) = open_scan_key(store, group_id, user_id, name).await?;
            show_scan_group(
                &ctx,
                store,
                scans,
                group_id,
                &canonical,
                &key,
                Some(index),
                false,
            )
            .await
        }
    }
}

async fn open_scan_key(
    store: &Store,
    group_id: i64,
    user_id: i64,
    name: &str,
) -> Result<(String, ScanKey), CommandError> {
    let canonical = store
        .resolve_name(group_id, name)
        .await
        .map_err(|error| missing_library(name, error))?;
    Ok((
        canonical.clone(),
        ScanKey {
            group_id,
            user_id,
            library: canonical,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
async fn show_scan_group(
    ctx: &CommandContext,
    store: &Store,
    scans: &ScanSessions,
    group_id: i64,
    library: &str,
    key: &ScanKey,
    jump: Option<usize>,
    starting: bool,
) -> CommandResult {
    loop {
        let advance = match jump {
            Some(index) => scans.jump(key, index),
            None => scans.advance(key),
        };
        match advance {
            None => {
                return Err(CommandError::user(format!("请先发送「查重 {library}」")));
            }
            Some(ScanAdvance::Exhausted) => {
                ctx.reply(if starting {
                    format!("「{library}」里没有相似的图")
                } else {
                    "没有下一组了".to_owned()
                });
                return Ok(());
            }
            Some(ScanAdvance::OutOfRange { total }) => {
                ctx.reply(if total == 0 {
                    format!("「{library}」里没有相似的图")
                } else {
                    format!("只有 {total} 组")
                });
                return Ok(());
            }
            Some(ScanAdvance::Group {
                group,
                index,
                total,
            }) => {
                let mut images = Vec::new();
                for hash in &group.hashes {
                    if let Ok(bytes) = store.read_blob(group_id, hash).await {
                        images.push(bytes);
                    }
                }
                if images.len() < 2 {
                    if jump.is_some() {
                        ctx.reply(format!("第 {index} 组不足两张，可能已经删过了"));
                        return Ok(());
                    }
                    continue;
                }
                reply_group(
                    ctx,
                    group_title(group.kind, index, total, group.percent),
                    images,
                );
                return Ok(());
            }
        }
    }
}

fn reply_group(ctx: &CommandContext, title: String, images: Vec<Vec<u8>>) {
    let packets = packetize_images(images);
    for (i, packet) in packets.into_iter().enumerate() {
        let mut message = if i == 0 {
            Message::new().add_text(&title)
        } else {
            Message::new().add_text("（续）")
        };
        for bytes in packet {
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            message = message.add_image(&format!("base64://{encoded}"));
        }
        ctx.reply(message);
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

fn quoted_library_names(libraries: &[String]) -> String {
    libraries
        .iter()
        .map(|name| format!("「{name}」"))
        .collect::<Vec<_>>()
        .join("")
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
        StoreError::HashAmbiguous => CommandError::user("哈希前缀对应多张图，请写长一点"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

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

    #[test]
    fn scan_op_parses_start_next_jump_percent_and_rejects_junk() {
        assert!(matches!(
            parse_scan_op(&args(&["猫"])),
            Ok(ScanOp::Start {
                name: "猫",
                percent: None
            })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "下一组"])),
            Ok(ScanOp::Next { .. })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "3"])),
            Ok(ScanOp::Jump { index: 3, .. })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "90%"])),
            Ok(ScanOp::Start {
                percent: Some(90),
                ..
            })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "foo"])),
            Err(CommandError::User(_))
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "0"])),
            Err(CommandError::User(_))
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "下一组", "3"])),
            Err(CommandError::UnexpectedArgument)
        ));
    }
}
