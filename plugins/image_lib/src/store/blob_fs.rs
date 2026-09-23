//! 图库文件层：blob 路径校验、原子入库与目录对账所需的盘上清单。

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use sqlx::SqlitePool;

use super::{StoreError, repo::hash_still_used};

pub(super) fn blob_file(blobs: &Path, hash: &str) -> Result<PathBuf> {
    if !is_blob_hash(hash) {
        anyhow::bail!("非法图片哈希");
    }
    Ok(blobs.join(hash))
}

pub(super) async fn blob_hashes_on_disk(blobs: &Path) -> Result<HashSet<String>, StoreError> {
    let mut entries = kovi::tokio::fs::read_dir(blobs)
        .await
        .context("列出图片目录失败")?;
    let mut disk = HashSet::new();
    while let Some(entry) = entries.next_entry().await.context("列出图片目录失败")? {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext == "tmp" {
                let _ = kovi::tokio::fs::remove_file(&path).await;
            }
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if is_blob_hash(name) {
            disk.insert(name.to_owned());
        }
    }
    Ok(disk)
}

fn is_blob_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn is_hash_prefix(prefix: &str) -> bool {
    let len = prefix.len();
    (1..=64).contains(&len) && prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// staged 文件入库：落盘、收紧权限后，在同目录内 rename 成正式 blob。
pub(super) async fn promote_staged(from: &Path, to: &Path) -> Result<()> {
    // rename 前先落盘：DB 里已有该 hash 的索引，掉电截断的 blob 会被对账
    // 当作正常文件，这张图就永久损坏了。
    let file = kovi::tokio::fs::OpenOptions::new()
        .write(true)
        .open(from)
        .await?;
    file.sync_all().await?;
    drop(file);
    utils::restrict_mode_0600(from)?;
    kovi::tokio::fs::rename(from, to).await?;
    Ok(())
}

/// 入库失败的补偿：删除已写成 blob 但没有任何库引用的文件。
pub(super) async fn remove_unindexed(
    pool: &SqlitePool,
    created: &[(String, PathBuf)],
) -> Result<(), StoreError> {
    for (hash, path) in created {
        if hash_still_used(pool, hash).await? {
            continue;
        }
        let _ = kovi::tokio::fs::remove_file(path).await;
    }
    Ok(())
}
