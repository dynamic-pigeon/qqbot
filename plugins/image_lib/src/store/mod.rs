//! 按群分片的图库存储：`<root>/<group_id>/index.db` 存元数据，
//! `blobs/<sha256>` 存内容寻址的图片文件。
//! SQL 在 [`repo`]，文件层在 [`blob_fs`]，建表迁移在 [`schema`]。

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use kovi::tokio::sync::{Mutex, mpsc};

use anyhow::{Context, Result};
use rand::seq::IndexedRandom;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::similar::{Fingerprint, HashedImage, fingerprint_bytes};

mod blob_fs;
mod repo;
mod schema;

#[cfg(test)]
mod tests;

use blob_fs::{blob_file, blob_hashes_on_disk, is_hash_prefix, promote_staged, remove_unindexed};
use repo::{
    additional_unique_bytes, delete_fingerprint, hash_still_used, hashes_only_in_library,
    insert_fingerprints, insert_images, library_exists, library_fingerprints, library_hashes,
    merge_library, prune_dangling_aliases, resolve_library, unique_image_bytes, upsert_alias,
};
use schema::init_schema;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("本群图库容量不足")]
    QuotaExceeded {
        used: u64,
        additional: u64,
        limit: u64,
    },
    #[error("库不存在")]
    LibraryMissing,
    #[error("没有这张图")]
    ImageMissing,
    #[error("哈希前缀对应多张图")]
    HashAmbiguous,
    #[error("库是空的")]
    LibraryEmpty,
    #[error("别名不能指向自己")]
    AliasToSelf,
    #[error("「{0}」已是图库，不能当别名；要合成一个请在末尾加「合并」")]
    NameIsLibrary(String),
    #[error("「{alias}」已是「{target}」的别名；要合成一个请在末尾加「合并」")]
    AliasTaken { alias: String, target: String },
    #[error("「{0}」不存在")]
    TargetMissing(String),
    #[error("「{0}」不是别名")]
    AliasMissing(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Other(error.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResult {
    pub added: usize,
    pub skipped_dup: usize,
}

/// 已下载到 blobs 目录旁、等待入库的图片：内容哈希、字节数与临时文件路径。
#[derive(Debug, Clone)]
pub struct StagedImage {
    pub hash: String,
    pub size: u64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasResult {
    pub canonical: String,
    pub merged_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryStat {
    pub name: String,
    pub aliases: Vec<String>,
    pub count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStats {
    pub libraries: Vec<LibraryStat>,
    pub unique_count: usize,
    pub unique_bytes: u64,
}

pub struct Store {
    root: PathBuf,
    max_group_bytes: u64,
    /// 群锁和连接池都在 async 路径上取，用 tokio Mutex 以免卡住 runtime。
    locks: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
    pools: Mutex<HashMap<i64, SqlitePool>>,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        Self::open_with_quota(root, crate::config::static_config().max_group_bytes())
    }

    pub(crate) fn open_with_quota(root: PathBuf, max_group_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建图库目录失败: {}", root.display()))?;
        Ok(Self {
            root,
            max_group_bytes,
            locks: Mutex::new(HashMap::new()),
            pools: Mutex::new(HashMap::new()),
        })
    }

    pub fn max_group_bytes(&self) -> u64 {
        self.max_group_bytes
    }

    async fn group_lock(&self, group_id: i64) -> Arc<Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(group_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn group_dir(&self, group_id: i64) -> PathBuf {
        self.root.join(group_id.to_string())
    }

    pub(crate) fn blobs_dir(&self, group_id: i64) -> PathBuf {
        self.group_dir(group_id).join("blobs")
    }

    fn db_path(&self, group_id: i64) -> PathBuf {
        self.group_dir(group_id).join("index.db")
    }

    fn blob_path(&self, group_id: i64, hash: &str) -> Result<PathBuf> {
        blob_file(&self.blobs_dir(group_id), hash)
    }

    async fn with_group<T, F, Fut>(&self, group_id: i64, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(SqlitePool) -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
    {
        let lock = self.group_lock(group_id).await;
        let _guard = lock.lock().await;
        let pool = self.ensure_pool(group_id).await?;
        f(pool).await
    }

    async fn ensure_pool(&self, group_id: i64) -> Result<SqlitePool, StoreError> {
        if let Some(pool) = self.pools.lock().await.get(&group_id).cloned() {
            return Ok(pool);
        }

        let dir = self.group_dir(group_id);
        kovi::tokio::fs::create_dir_all(&dir)
            .await
            .context("创建群图库目录失败")?;
        kovi::tokio::fs::create_dir_all(self.blobs_dir(group_id))
            .await
            .context("创建图片目录失败")?;
        let db_path = self.db_path(group_id);
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        init_schema(&pool).await?;
        utils::restrict_mode_0600(&db_path).context("收紧图库数据库权限失败")?;
        self.pools.lock().await.insert(group_id, pool.clone());
        Ok(pool)
    }

    pub async fn add_images(
        &self,
        group_id: i64,
        name: &str,
        images: Vec<StagedImage>,
    ) -> Result<AddResult, StoreError> {
        let blobs = self.blobs_dir(group_id);
        let max_group_bytes = self.max_group_bytes;
        let result = self
            .with_group(group_id, |pool| {
                let images = &images;
                async move {
                    let library = resolve_library(&pool, name).await?;
                    let existing = library_hashes(&pool, &library).await?;

                    let mut added_hashes = HashSet::new();
                    let mut to_insert = Vec::new();
                    let mut skipped_dup = 0usize;

                    for image in images {
                        if existing.contains(&image.hash)
                            || !added_hashes.insert(image.hash.clone())
                        {
                            skipped_dup += 1;
                            continue;
                        }
                        to_insert.push(image);
                    }

                    let additional = additional_unique_bytes(&pool, &to_insert).await?;
                    let used = unique_image_bytes(&pool).await?;
                    if used.saturating_add(additional) > max_group_bytes {
                        return Err(StoreError::QuotaExceeded {
                            used,
                            additional,
                            limit: max_group_bytes,
                        });
                    }

                    let mut created = Vec::new();
                    for image in &to_insert {
                        let path = blob_file(&blobs, &image.hash)?;
                        if kovi::tokio::fs::try_exists(&path).await.unwrap_or(false) {
                            continue;
                        }
                        if let Err(error) = promote_staged(&image.path, &path).await {
                            let _ = remove_unindexed(&pool, &created).await;
                            return Err(error.into());
                        }
                        created.push((image.hash.clone(), path));
                    }

                    if let Err(error) = insert_images(&pool, &library, &to_insert).await {
                        let _ = remove_unindexed(&pool, &created).await;
                        return Err(error);
                    }

                    Ok(AddResult {
                        added: to_insert.len(),
                        skipped_dup,
                    })
                }
            })
            .await;
        // staged 文件被 promote 后原路径已不存在，剩余的（重复跳过、
        // blob 复用、出错回滚）在此统一清理。
        for image in &images {
            let _ = kovi::tokio::fs::remove_file(&image.path).await;
        }
        result
    }

    pub async fn delete_hash(&self, group_id: i64, hash: &str) -> Result<Vec<String>, StoreError> {
        let blobs = self.blobs_dir(group_id);
        self.with_group(group_id, |pool| async move {
            let mut libraries = sqlx::query_scalar::<_, String>(
                "DELETE FROM images WHERE hash = ? RETURNING library",
            )
            .bind(hash)
            .fetch_all(&pool)
            .await?;
            if libraries.is_empty() {
                return Err(StoreError::ImageMissing);
            }
            libraries.sort();
            libraries.dedup();
            prune_dangling_aliases(&pool).await?;
            if !hash_still_used(&pool, hash).await?
                && let Ok(path) = blob_file(&blobs, hash)
            {
                let _ = kovi::tokio::fs::remove_file(path).await;
                delete_fingerprint(&pool, hash).await?;
            }
            Ok(libraries)
        })
        .await
    }

    pub async fn wipe_library(&self, group_id: i64, name: &str) -> Result<String, StoreError> {
        let blobs = self.blobs_dir(group_id);
        self.with_group(group_id, |pool| async move {
            let canonical = resolve_library(&pool, name).await?;
            if !library_exists(&pool, &canonical).await? {
                return Err(StoreError::LibraryMissing);
            }
            let exclusive = hashes_only_in_library(&pool, &canonical).await?;
            let mut tx = pool.begin().await?;
            sqlx::query(
                "DELETE FROM perceptual WHERE hash IN (
                     SELECT mine.hash FROM images AS mine
                     WHERE mine.library = ?
                       AND NOT EXISTS (
                           SELECT 1 FROM images AS other
                           WHERE other.hash = mine.hash AND other.library != mine.library
                       )
                 )",
            )
            .bind(&canonical)
            .execute(&mut *tx)
            .await?;
            sqlx::query("DELETE FROM images WHERE library = ?")
                .bind(&canonical)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM aliases WHERE target = ? OR alias = ?")
                .bind(&canonical)
                .bind(&canonical)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            for hash in exclusive {
                if let Ok(path) = blob_file(&blobs, &hash) {
                    let _ = kovi::tokio::fs::remove_file(path).await;
                }
            }
            Ok(canonical)
        })
        .await
    }

    pub async fn pick_random(&self, group_id: i64, name: &str) -> Result<String, StoreError> {
        self.with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            let rows = sqlx::query("SELECT hash, draw_count FROM images WHERE library = ?")
                .bind(&library)
                .fetch_all(&pool)
                .await?;
            let items = rows
                .into_iter()
                .map(|row| {
                    Ok((
                        row.try_get::<String, _>("hash")?,
                        row.try_get::<i64, _>("draw_count")?,
                    ))
                })
                .collect::<Result<Vec<_>, sqlx::Error>>()?;
            let hash = pick_weighted(&items)
                .map(str::to_owned)
                .ok_or(StoreError::LibraryEmpty)?;
            sqlx::query(
                "UPDATE images SET draw_count = draw_count + 1 WHERE library = ? AND hash = ?",
            )
            .bind(&library)
            .bind(&hash)
            .execute(&pool)
            .await?;
            Ok(hash)
        })
        .await
    }

    pub async fn set_alias(
        &self,
        group_id: i64,
        alias: &str,
        target: &str,
        merge: bool,
    ) -> Result<AliasResult, StoreError> {
        self.with_group(group_id, |pool| async move {
            let canonical = resolve_library(&pool, target).await?;
            if alias == canonical {
                return Err(StoreError::AliasToSelf);
            }
            if !library_exists(&pool, &canonical).await? {
                return Err(StoreError::TargetMissing(target.to_owned()));
            }

            let alias_is_library = library_exists(&pool, alias).await?;
            let existing_target =
                sqlx::query_scalar::<_, String>("SELECT target FROM aliases WHERE alias = ?")
                    .bind(alias)
                    .fetch_optional(&pool)
                    .await?;

            if !merge {
                if alias_is_library {
                    return Err(StoreError::NameIsLibrary(alias.to_owned()));
                }
                // 已占用的别名必须先取消或加「合并」，避免悄悄换库。
                if let Some(existing_target) = existing_target {
                    if existing_target == canonical {
                        return Ok(AliasResult {
                            canonical,
                            merged_from: None,
                        });
                    }
                    return Err(StoreError::AliasTaken {
                        alias: alias.to_owned(),
                        target: existing_target,
                    });
                }
                sqlx::query("INSERT INTO aliases (alias, target) VALUES (?, ?)")
                    .bind(alias)
                    .bind(&canonical)
                    .execute(&pool)
                    .await?;
                return Ok(AliasResult {
                    canonical,
                    merged_from: None,
                });
            }

            let source = if alias_is_library {
                Some(alias.to_owned())
            } else {
                existing_target
            };
            let merge_source = if let Some(source) = source.as_deref() {
                if source != canonical && library_exists(&pool, source).await? {
                    Some(source.to_owned())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(from) = merge_source {
                let mut tx = pool.begin().await?;
                merge_library(&mut tx, &from, &canonical).await?;
                upsert_alias(&mut *tx, alias, &canonical).await?;
                tx.commit().await?;
                return Ok(AliasResult {
                    canonical,
                    merged_from: Some(from),
                });
            }

            upsert_alias(&pool, alias, &canonical).await?;
            Ok(AliasResult {
                canonical,
                merged_from: None,
            })
        })
        .await
    }

    pub async fn remove_alias(&self, group_id: i64, alias: &str) -> Result<(), StoreError> {
        self.with_group(group_id, |pool| async move {
            let result = sqlx::query("DELETE FROM aliases WHERE alias = ?")
                .bind(alias)
                .execute(&pool)
                .await?;
            if result.rows_affected() == 0 {
                return Err(StoreError::AliasMissing(alias.to_owned()));
            }
            Ok(())
        })
        .await
    }

    pub async fn stats(&self, group_id: i64) -> Result<GroupStats, StoreError> {
        self.with_group(group_id, |pool| async move {
            let rows = sqlx::query(
                "SELECT library, COUNT(*) AS count, SUM(size) AS bytes
                 FROM images GROUP BY library ORDER BY library",
            )
            .fetch_all(&pool)
            .await?;
            let alias_rows = sqlx::query("SELECT alias, target FROM aliases ORDER BY alias")
                .fetch_all(&pool)
                .await?;
            let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for row in alias_rows {
                let alias: String = row.try_get("alias")?;
                let target: String = row.try_get("target")?;
                aliases.entry(target).or_default().push(alias);
            }
            let libraries = rows
                .into_iter()
                .map(|row| {
                    let name: String = row.try_get("library")?;
                    Ok(LibraryStat {
                        aliases: aliases.remove(&name).unwrap_or_default(),
                        name,
                        count: row.try_get::<i64, _>("count")? as usize,
                        bytes: row.try_get::<i64, _>("bytes")? as u64,
                    })
                })
                .collect::<Result<Vec<_>, sqlx::Error>>()?;
            let unique_count =
                sqlx::query_scalar::<_, i64>("SELECT COUNT(DISTINCT hash) FROM images")
                    .fetch_one(&pool)
                    .await? as usize;
            let unique_bytes = unique_image_bytes(&pool).await?;
            Ok(GroupStats {
                libraries,
                unique_count,
                unique_bytes,
            })
        })
        .await
    }

    /// 本群已入库哈希：完整 64 位或能唯一确定一张图的前缀。
    pub async fn resolve_group_hash(
        &self,
        group_id: i64,
        prefix: &str,
    ) -> Result<String, StoreError> {
        let prefix = prefix.to_ascii_lowercase();
        if !is_hash_prefix(&prefix) {
            return Err(StoreError::ImageMissing);
        }
        self.with_group(group_id, |pool| async move {
            let pattern = format!("{prefix}%");
            let hashes = sqlx::query_scalar::<_, String>(
                "SELECT DISTINCT hash FROM images WHERE hash LIKE ? ESCAPE '\\'",
            )
            .bind(&pattern)
            .fetch_all(&pool)
            .await?;
            match hashes.as_slice() {
                [hash] => Ok(hash.clone()),
                [] => Err(StoreError::ImageMissing),
                _ => Err(StoreError::HashAmbiguous),
            }
        })
        .await
    }

    /// 解析前缀后读出 blob，供管理员按哈希发图。
    pub async fn load_by_hash_prefix(
        &self,
        group_id: i64,
        prefix: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let hash = self.resolve_group_hash(group_id, prefix).await?;
        self.read_blob(group_id, &hash)
            .await
            .map_err(StoreError::Other)
    }

    /// 解析前缀后从本群所有库删掉该图。
    pub async fn delete_by_hash_prefix(
        &self,
        group_id: i64,
        prefix: &str,
    ) -> Result<Vec<String>, StoreError> {
        let hash = self.resolve_group_hash(group_id, prefix).await?;
        self.delete_hash(group_id, &hash).await
    }

    pub async fn resolve_name(&self, group_id: i64, name: &str) -> Result<String, StoreError> {
        self.with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            if !library_exists(&pool, &library).await? {
                return Err(StoreError::LibraryMissing);
            }
            Ok(library)
        })
        .await
    }

    pub async fn read_blob(&self, group_id: i64, hash: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(group_id, hash)?;
        kovi::tokio::fs::read(&path)
            .await
            .with_context(|| format!("读取图片失败: {}", path.display()))
    }

    /// 解析库名（含别名），补齐缺失的感知哈希后返回可比较的图。
    pub async fn fingerprints_for_library(
        &self,
        group_id: i64,
        name: &str,
    ) -> Result<(String, Vec<HashedImage>), StoreError> {
        let blobs = self.blobs_dir(group_id);
        self.with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            if !library_exists(&pool, &library).await? {
                return Err(StoreError::LibraryMissing);
            }
            let hashes: Vec<String> = library_hashes(&pool, &library).await?.into_iter().collect();
            let mut fingerprints = library_fingerprints(&pool, &library).await?;
            let missing: Vec<(String, PathBuf)> = hashes
                .iter()
                .filter(|hash| !fingerprints.contains_key(*hash))
                .filter_map(|hash| {
                    blob_file(&blobs, hash)
                        .ok()
                        .map(|path| (hash.clone(), path))
                })
                .collect();

            let computed = fingerprint_missing(missing).await?;
            insert_fingerprints(&pool, &computed).await?;
            for (hash, fingerprint) in computed {
                fingerprints.insert(hash, fingerprint);
            }

            let images = hashes
                .into_iter()
                .filter_map(|hash| {
                    fingerprints
                        .remove(&hash)
                        .map(|fingerprint| HashedImage { hash, fingerprint })
                })
                .collect();
            Ok((library, images))
        })
        .await
    }

    pub(crate) async fn reconcile_all(&self) {
        let groups = match self.list_group_ids().await {
            Ok(groups) => groups,
            Err(error) => {
                tracing::error!("列举图库群目录失败: {error}");
                return;
            }
        };
        for group_id in groups {
            if let Err(error) = self.reconcile_group(group_id).await {
                tracing::error!("图库对账失败 group_id={group_id}: {error}");
            }
        }
    }

    async fn list_group_ids(&self) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        let mut entries = kovi::tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            if let Ok(id) = entry.file_name().to_string_lossy().parse::<i64>() {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    async fn reconcile_group(&self, group_id: i64) -> Result<(), StoreError> {
        let blobs = self.blobs_dir(group_id);
        self.with_group(group_id, |pool| async move {
            let disk = blob_hashes_on_disk(&blobs).await?;

            let indexed: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT hash FROM images")
                .fetch_all(&pool)
                .await?
                .into_iter()
                .collect();

            let mut removed_files = 0u64;
            for hash in disk.difference(&indexed) {
                if let Ok(path) = blob_file(&blobs, hash)
                    && kovi::tokio::fs::remove_file(&path).await.is_ok()
                {
                    removed_files += 1;
                }
            }

            let mut tx = pool.begin().await?;
            let mut removed_rows = 0u64;
            for hash in indexed.difference(&disk) {
                let result = sqlx::query("DELETE FROM images WHERE hash = ?")
                    .bind(hash)
                    .execute(&mut *tx)
                    .await?;
                removed_rows += result.rows_affected();
            }
            if removed_rows > 0 {
                sqlx::query(
                    "DELETE FROM aliases WHERE target NOT IN (SELECT DISTINCT library FROM images)",
                )
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(
                "DELETE FROM perceptual WHERE hash NOT IN (SELECT DISTINCT hash FROM images)",
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;

            if removed_files > 0 || removed_rows > 0 {
                tracing::info!(group_id, removed_files, removed_rows, "图库对账完成");
            }
            Ok(())
        })
        .await
    }
}

/// 读盘走 async，解码只占一条 blocking 线程。
/// 通道容量 1：进行中和解码排队的各一张，峰值大约两张 blob。
async fn fingerprint_missing(
    missing: Vec<(String, PathBuf)>,
) -> Result<Vec<(String, Fingerprint)>, StoreError> {
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let (tx, mut rx) = mpsc::channel::<(String, Vec<u8>)>(1);
    let worker = kovi::tokio::task::spawn_blocking(move || {
        let mut computed = Vec::new();
        while let Some((hash, bytes)) = rx.blocking_recv() {
            if let Some(fingerprint) = fingerprint_bytes(&bytes) {
                computed.push((hash, fingerprint));
            }
        }
        computed
    });
    for (hash, path) in missing {
        let Ok(bytes) = kovi::tokio::fs::read(path).await else {
            continue;
        };
        if tx.send((hash, bytes)).await.is_err() {
            break;
        }
    }
    drop(tx);
    worker
        .await
        .map_err(|e| StoreError::Other(anyhow::anyhow!("计算感知哈希失败: {e}")))
}

/// 权重 `4096 >> (次数 - 库内最小次数)`，最少的那档是 4096，最多落后 12 次仍为 1。
fn pick_weighted(items: &[(String, i64)]) -> Option<&str> {
    let min = items.iter().map(|(_, count)| *count).min()?;
    items
        .choose_weighted(&mut rand::rng(), |(_, count)| weight(*count, min))
        .ok()
        .map(|(hash, _)| hash.as_str())
}

fn weight(count: i64, min: i64) -> u32 {
    const MAX_WEIGHT: u32 = 1 << 12;
    MAX_WEIGHT >> (count.saturating_sub(min) as u32).min(12)
}
