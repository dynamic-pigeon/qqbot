use std::sync::Arc;

use utils::command::{
    Command, CommandContext, CommandError, CommandResult, MessageScope, Permission,
};

use crate::name::parse_library_name;
use crate::scan::{
    GROUPS_PER_PAGE, NEXT_PAGE_ARG, PackedImage, ScanAdvance, ScanKey, ScanSessions,
    chunk_forward_packets, group_node_name, group_title, packets_for_group, parse_group_index,
};
use crate::send::{
    OutgoingImage, forward_node, image_message, report_send_fail, send_group_forward_wait,
};
use crate::similar::{SimilarGroup, cluster, distance_from_percent};
use crate::store::{CropPlan, Store};

use super::missing_library;

/// 查重展示单组读入内存的原始字节预算。大组逐张全读可放大到数百 MiB 常驻。
#[cfg(not(feature = "send-file"))]
const MAX_GROUP_READ_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanKind {
    Duplicate,
    Crop,
}

impl ScanKind {
    fn hint(self) -> &'static str {
        match self {
            Self::Duplicate => "查重",
            Self::Crop => "查裁剪",
        }
    }
}

pub(super) fn scan_command(store: Arc<Store>, scans: Arc<ScanSessions>) -> Command {
    Command::new("查重")
        .description("扫指定图库的近重复，聊天记录每次最多 5 组")
        .usage("查重 <库名> [组号|下一组|相似度%]")
        .permission(Permission::BotAdmin)
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let scans = Arc::clone(&scans);
            async move { handle_scan(ctx, &store, &scans, ScanKind::Duplicate).await }
        })
}

pub(super) fn crop_scan_command(store: Arc<Store>, scans: Arc<ScanSessions>) -> Command {
    Command::new("查裁剪")
        .description("扫指定图库的裁剪局部对，先整体后局部，每次最多 5 组")
        .usage("查裁剪 <库名> [组号|下一组]")
        .permission(Permission::BotAdmin)
        .scope(MessageScope::Group)
        .group("图库")
        .prefix_match()
        .handler(move |ctx| {
            let store = Arc::clone(&store);
            let scans = Arc::clone(&scans);
            async move { handle_scan(ctx, &store, &scans, ScanKind::Crop).await }
        })
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

fn parse_scan_args(args: &[String]) -> Result<ScanOp<'_>, CommandError> {
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

/// 查重接受相似度百分比。查裁剪复用同一套组号和「下一组」，但拒绝百分比，
/// 并把其余解析失败收成裁剪自己的提示。
fn parse_scan_op(args: &[String], kind: ScanKind) -> Result<ScanOp<'_>, CommandError> {
    let op = parse_scan_args(args).map_err(|error| match kind {
        ScanKind::Duplicate => error,
        ScanKind::Crop => match error {
            error @ CommandError::MissingArgument { .. } => error,
            _ => CommandError::user("第二参数是组号或下一组"),
        },
    })?;
    if kind == ScanKind::Crop
        && let ScanOp::Start {
            percent: Some(_), ..
        } = &op
    {
        return Err(CommandError::user(
            "查裁剪不支持相似度%；第二参数是组号或下一组",
        ));
    }
    Ok(op)
}

async fn handle_scan(
    ctx: CommandContext,
    store: &Store,
    scans: &ScanSessions,
    kind: ScanKind,
) -> CommandResult {
    let op = parse_scan_op(ctx.args(), kind)?;
    let group_id = ctx.group_id()?;
    let user_id = ctx.event().user_id;
    let hint = kind.hint();
    match op {
        ScanOp::Start { name, percent } => {
            let Some((canonical, groups)) =
                begin_scan(&ctx, store, group_id, name, percent, kind).await?
            else {
                return Ok(());
            };
            let key = ScanKey {
                group_id,
                user_id,
                library: canonical.clone(),
            };
            scans.start(key.clone(), groups);
            ScanPage {
                ctx: &ctx,
                store,
                scans,
                group_id,
                library: &canonical,
                key: &key,
                command_hint: hint,
            }
            .show(None, true)
            .await
        }
        ScanOp::Next { name } => resume_scan(&ctx, store, scans, group_id, name, None, hint).await,
        ScanOp::Jump { name, index } => {
            resume_scan(&ctx, store, scans, group_id, name, Some(index), hint).await
        }
    }
}

/// 开一轮扫描。已经回复过（库太空、没有结果）时返回 `None`。
async fn begin_scan(
    ctx: &CommandContext,
    store: &Store,
    group_id: i64,
    name: &str,
    percent: Option<u32>,
    kind: ScanKind,
) -> Result<Option<(String, Vec<SimilarGroup>)>, CommandError> {
    match kind {
        ScanKind::Duplicate => begin_duplicate(ctx, store, group_id, name, percent).await,
        ScanKind::Crop => begin_crop(ctx, store, group_id, name).await,
    }
}

async fn begin_duplicate(
    ctx: &CommandContext,
    store: &Store,
    group_id: i64,
    name: &str,
    percent: Option<u32>,
) -> Result<Option<(String, Vec<SimilarGroup>)>, CommandError> {
    let (canonical, images) = store
        .fingerprints_for_library(group_id, name)
        .await
        .map_err(|error| missing_library(name, error))?;
    if images.len() < 2 {
        ctx.reply(format!("「{canonical}」里没有相似的图"));
        return Ok(None);
    }
    let config = crate::config::static_config();
    let duplicate = percent
        .map(distance_from_percent)
        .unwrap_or_else(|| config.duplicate_distance());
    // 聚类是纯 CPU 的 O(n²) 比较，大库会占住唯一的 async worker 线程。
    let groups = kovi::tokio::task::spawn_blocking(move || {
        cluster(&images, duplicate, config.maybe_distance())
    })
    .await
    .map_err(|e| CommandError::internal(anyhow::anyhow!("查重计算线程失败: {e}")))?;
    if groups.is_empty() {
        ctx.reply(format!("「{canonical}」里没有相似的图"));
        return Ok(None);
    }
    Ok(Some((canonical, groups)))
}

async fn begin_crop(
    ctx: &CommandContext,
    store: &Store,
    group_id: i64,
    name: &str,
) -> Result<Option<(String, Vec<SimilarGroup>)>, CommandError> {
    let (canonical, plan) = store
        .crop_scan_prepare(group_id, name)
        .await
        .map_err(|error| missing_library(name, error))?;
    // 缓存全命中时不回提示，结果直接出；只有真要算的库才说「别急」。
    let mut had_coverage = false;
    if let CropPlan::Incremental {
        had_coverage: covered,
        ..
    } = &plan
    {
        had_coverage = *covered;
        ctx.reply(if *covered {
            format!("正在补算「{canonical}」新增图片的裁剪比对，稍等")
        } else {
            format!(
                "正在全量扫「{canonical}」的裁剪局部，每张图都要和全库两两比对，大库要几分钟，别急"
            )
        });
    }
    let groups = store
        .crop_scan_run(group_id, plan)
        .await
        .map_err(CommandError::internal)?;
    if groups.is_empty() {
        ctx.reply(format!("「{canonical}」里没有裁剪关系的图"));
        return Ok(None);
    }
    if had_coverage {
        send_fresh_crop_groups(ctx, store, group_id, &canonical, &groups).await?;
    }
    Ok(Some((canonical, groups)))
}

/// 补算检出新对时，把本轮新增的裁剪对在全新量列表之前单独发一份。
/// 首扫缓存从空起步、全量即新增，不再重复发一份。
async fn send_fresh_crop_groups(
    ctx: &CommandContext,
    store: &Store,
    group_id: i64,
    library: &str,
    groups: &[SimilarGroup],
) -> CommandResult {
    let fresh: Vec<&SimilarGroup> = groups.iter().filter(|group| group.fresh).collect();
    if fresh.is_empty() {
        return Ok(());
    }
    let total = fresh.len();
    let mut page = Vec::with_capacity(total);
    for (offset, group) in fresh.iter().enumerate() {
        let index = offset + 1;
        let (images, read_cut) = read_group_images(store, group_id, &group.hashes).await;
        if images.len() < 2 {
            continue;
        }
        let truncated = group.truncated || read_cut;
        page.push(ScanPageGroup {
            title: format!(
                "新增{}",
                group_title(group.kind, index, total, group.percent, truncated)
            ),
            name: format!(
                "新增{}",
                group_node_name(group.kind, index, total, group.percent, truncated)
            ),
            images,
            index,
        });
    }
    reply_scan_page(ctx, store, library, page, total).await
}

/// 恢复进行中的扫描会话并展示对应页；`jump` 是组号，`None` 表示翻下一组。
async fn resume_scan(
    ctx: &CommandContext,
    store: &Store,
    scans: &ScanSessions,
    group_id: i64,
    name: &str,
    jump: Option<usize>,
    command_hint: &str,
) -> CommandResult {
    let (canonical, key) = open_scan_key(store, group_id, ctx.event().user_id, name).await?;
    ScanPage {
        ctx,
        store,
        scans,
        group_id,
        library: &canonical,
        key: &key,
        command_hint,
    }
    .show(jump, false)
    .await
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

struct ScanPageGroup {
    title: String,
    name: String,
    images: Vec<PackedImage>,
    index: usize,
}

struct ScanPage<'a> {
    ctx: &'a CommandContext,
    store: &'a Store,
    scans: &'a ScanSessions,
    group_id: i64,
    library: &'a str,
    key: &'a ScanKey,
    command_hint: &'a str,
}

impl ScanPage<'_> {
    async fn show(self, mut jump: Option<usize>, starting: bool) -> CommandResult {
        let ScanPage {
            ctx,
            store,
            scans,
            group_id,
            library,
            key,
            command_hint,
        } = self;
        let mut page = Vec::new();
        let mut total = 0usize;
        loop {
            if page.len() >= GROUPS_PER_PAGE {
                break;
            }
            let from_jump = jump.is_some();
            let advance = match jump.take() {
                Some(index) => scans.jump(key, index),
                None => scans.advance(key),
            };
            match advance {
                None => {
                    if page.is_empty() {
                        return Err(CommandError::user(format!(
                            "请先发送「{command_hint} {library}」"
                        )));
                    }
                    break;
                }
                Some(ScanAdvance::Exhausted) => {
                    if page.is_empty() {
                        ctx.reply(if starting {
                            format!("「{library}」里没有相似的图")
                        } else {
                            "没有下一组了".to_owned()
                        });
                        return Ok(());
                    }
                    break;
                }
                Some(ScanAdvance::OutOfRange { total }) => {
                    if page.is_empty() {
                        ctx.reply(if total == 0 {
                            format!("「{library}」里没有相似的图")
                        } else {
                            format!("只有 {total} 组")
                        });
                        return Ok(());
                    }
                    break;
                }
                Some(ScanAdvance::Group {
                    group,
                    index,
                    total: group_total,
                }) => {
                    total = group_total;
                    let (images, read_cut) =
                        read_group_images(store, group_id, &group.hashes).await;
                    if images.len() < 2 {
                        if from_jump && page.is_empty() {
                            ctx.reply(format!("第 {index} 组不足两张，可能已经删过了"));
                            return Ok(());
                        }
                        continue;
                    }
                    let truncated = group.truncated || read_cut;
                    page.push(ScanPageGroup {
                        title: group_title(
                            group.kind,
                            index,
                            group_total,
                            group.percent,
                            truncated,
                        ),
                        name: group_node_name(
                            group.kind,
                            index,
                            group_total,
                            group.percent,
                            truncated,
                        ),
                        images,
                        index,
                    });
                }
            }
        }
        reply_scan_page(ctx, store, library, page, total).await
    }
}

/// 在 [`MAX_GROUP_READ_BYTES`] 预算内逐张读入组员图片；预算耗尽即止，
/// 返回是否被预算截断（组员全读会把大组放大成数百 MiB 常驻）。
/// send-file 形态不读内容、只 stat 路径，无内存压力，不做预算截断。
#[cfg(not(feature = "send-file"))]
async fn read_group_images(
    store: &Store,
    group_id: i64,
    hashes: &[String],
) -> (Vec<PackedImage>, bool) {
    let mut budget = MAX_GROUP_READ_BYTES;
    let mut read_cut = false;
    let mut images = Vec::new();
    for hash in hashes {
        if budget == 0 {
            read_cut = true;
            break;
        }
        if let Ok(bytes) = store.read_blob(group_id, hash).await {
            budget = budget.saturating_sub(bytes.len());
            images.push(PackedImage {
                hash: hash.clone(),
                image: OutgoingImage::from_bytes(bytes),
            });
        }
    }
    (images, read_cut)
}

#[cfg(feature = "send-file")]
async fn read_group_images(
    store: &Store,
    group_id: i64,
    hashes: &[String],
) -> (Vec<PackedImage>, bool) {
    let mut images = Vec::new();
    for hash in hashes {
        if let Some(image) = OutgoingImage::from_blob(store, group_id, hash).await {
            images.push(PackedImage {
                hash: hash.clone(),
                image,
            });
        }
    }
    (images, false)
}

async fn reply_scan_page(
    ctx: &CommandContext,
    store: &Store,
    library: &str,
    groups: Vec<ScanPageGroup>,
    group_total: usize,
) -> CommandResult {
    let group_id = ctx.group_id()?;
    let self_id = ctx.event().self_id;
    let from = groups.first().map(|group| group.index).unwrap_or(1);
    let to = groups.last().map(|group| group.index).unwrap_or(from);
    let mut grouped = Vec::new();
    for group in groups {
        grouped.push(packets_for_group(group.title, group.name, group.images));
    }
    let chunks = chunk_forward_packets(grouped);
    for (i, chunk) in chunks.iter().enumerate() {
        let nodes: Vec<_> = chunk
            .iter()
            .map(|packet| {
                let images = packet.images.iter().map(|image| &image.image);
                let message = image_message(Some(&packet.caption), images);
                forward_node(&packet.name, self_id, &message)
            })
            .collect();
        if let Err(error) = send_group_forward_wait(ctx.bot(), group_id, &nodes).await {
            let remaining: Vec<String> = chunks[i..]
                .iter()
                .flat_map(|part| {
                    part.iter()
                        .flat_map(|packet| packet.images.iter().map(|image| image.hash.clone()))
                })
                .collect();
            report_send_fail(
                ctx.bot(),
                store,
                group_id,
                format!(
                    "图库查重发送失败 group={group_id} 库={library} 组={from}-{to}/{group_total} 聊天记录={}",
                    i + 1
                ),
                &remaining,
                &error,
            )
            .await;
            return Err(CommandError::user(format!(
                "第 {} 条聊天记录发送失败",
                i + 1
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn crop_op_reuses_scan_op_but_rejects_percent() {
        assert!(matches!(
            parse_scan_op(&args(&["猫"]), ScanKind::Crop),
            Ok(ScanOp::Start {
                name: "猫",
                percent: None
            })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "下一组"]), ScanKind::Crop),
            Ok(ScanOp::Next { .. })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "3"]), ScanKind::Crop),
            Ok(ScanOp::Jump { index: 3, .. })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "90%"]), ScanKind::Crop),
            Err(CommandError::User(_))
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "foo"]), ScanKind::Crop),
            Err(CommandError::User(_))
        ));
    }

    #[test]
    fn scan_op_parses_start_next_jump_percent_and_rejects_junk() {
        assert!(matches!(
            parse_scan_op(&args(&["猫"]), ScanKind::Duplicate),
            Ok(ScanOp::Start {
                name: "猫",
                percent: None
            })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "下一组"]), ScanKind::Duplicate),
            Ok(ScanOp::Next { .. })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "3"]), ScanKind::Duplicate),
            Ok(ScanOp::Jump { index: 3, .. })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "90%"]), ScanKind::Duplicate),
            Ok(ScanOp::Start {
                percent: Some(90),
                ..
            })
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "foo"]), ScanKind::Duplicate),
            Err(CommandError::User(_))
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "0"]), ScanKind::Duplicate),
            Err(CommandError::User(_))
        ));
        assert!(matches!(
            parse_scan_op(&args(&["猫", "下一组", "3"]), ScanKind::Duplicate),
            Err(CommandError::UnexpectedArgument)
        ));
    }
}
