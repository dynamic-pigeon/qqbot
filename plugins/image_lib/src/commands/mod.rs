mod hash;
mod library;
mod scan;

use std::sync::Arc;

use utils::RateLimiter;
use utils::command::{Command, CommandContext, CommandError};

use crate::confirm::{ConfirmKey, WipeConfirmations};
use crate::scan::ScanSessions;
use crate::store::{Store, StoreError};

/// 图库命令集：各工厂自带群聊范围与「图库」分组，这里只决定注册顺序；
/// 分组在 /help 顶层折成一行入口（/help 图库 展开）。
pub fn image_lib_commands(store: Arc<Store>, limiter: Arc<RateLimiter<i64>>) -> Vec<Command> {
    let scans = Arc::new(ScanSessions::new());
    // 裁剪会话与查重会话同库同人也互不覆盖：两个命令各自翻页。
    let crop_scans = Arc::new(ScanSessions::new());
    let wipes = Arc::new(WipeConfirmations::new());
    vec![
        library::list_command(Arc::clone(&store), Arc::clone(&wipes)),
        library::add_command(Arc::clone(&store)),
        library::draw_command(Arc::clone(&store), limiter),
        library::delete_command(Arc::clone(&store), Arc::clone(&wipes)),
        library::alias_command(Arc::clone(&store)),
        library::unalias_command(Arc::clone(&store)),
        hash::send_hash_command(Arc::clone(&store)),
        hash::delete_hash_command(Arc::clone(&store)),
        scan::scan_command(Arc::clone(&store), scans),
        scan::crop_scan_command(store, crop_scans),
    ]
}

pub(super) fn confirm_key(ctx: &CommandContext, group_id: i64) -> ConfirmKey {
    ConfirmKey {
        group_id,
        user_id: ctx.event().user_id,
    }
}

pub(super) fn missing_library(name: &str, error: StoreError) -> CommandError {
    match error {
        StoreError::LibraryMissing => CommandError::user(format!("「{name}」不存在")),
        other => CommandError::internal(other),
    }
}

pub(super) fn quoted_library_names(libraries: &[String]) -> String {
    libraries
        .iter()
        .map(|name| format!("「{name}」"))
        .collect::<Vec<_>>()
        .join("")
}

pub(super) fn map_store_user_error(error: StoreError) -> CommandError {
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
        | StoreError::AliasTaken { .. }
        | StoreError::TargetMissing(_)
        | StoreError::AliasMissing(_) => CommandError::user(error.to_string()),
        other => CommandError::internal(other),
    }
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
