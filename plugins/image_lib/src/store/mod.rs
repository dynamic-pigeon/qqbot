//! 按群分片的图库存储：`<root>/<group_id>/index.db` 存元数据，
//! `blobs/<sha256>` 存内容寻址的图片文件。
//! SQL 在 [`repo`]，文件层在 [`blob_fs`]，建表迁移在 [`schema`]。

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use kovi::tokio::sync::Mutex;

use anyhow::{Context, Result};
use rand::seq::IndexedRandom;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::similar::{
    HashedImage, SiftableImage, SimilarGroup, assemble_crop_groups, crop_group, detect_crops,
    fingerprint_and_sift, fingerprint_bytes,
};

mod backup;
mod blob_fs;
mod repo;
mod schema;

#[cfg(test)]
mod tests;

use blob_fs::{blob_file, blob_hashes_on_disk, is_hash_prefix, promote_staged, remove_unindexed};
use repo::{
    additional_unique_bytes, backed_up_hashes, crop_covered, delete_crop_pairs_for_hash,
    delete_crop_state, delete_fingerprint, delete_sift, hash_still_used, insert_crop_pairs,
    insert_fingerprints, insert_images, insert_sifts, invalidate_crop_covered, library_crop_pairs,
    library_exists, library_fingerprints, library_hashes, library_sifts, merge_library,
    prune_dangling_aliases, purge_expired_backup_refs, resolve_library, save_crop_covered,
    unique_image_bytes, upsert_alias,
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

/// 清空整库前的确认信息：库名已解析到规范名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryOverview {
    pub canonical: String,
    pub count: usize,
    pub bytes: u64,
}

/// 「查裁剪」的执行计划。配对缓存全命中时无需任何计算；否则只补算
/// 两端任一不在覆盖集里的对。
#[derive(Debug)]
pub enum CropPlan {
    /// 覆盖完整，结果已从缓存的正结果对拼出。
    Complete(Vec<SimilarGroup>),
    Incremental {
        library: String,
        /// 本轮扫完写进覆盖标记的成员全集（含提不出特征的图，它们的对
        /// 永远算不出来，记入覆盖避免每次重试）。
        members: Vec<String>,
        /// 已比对过的图集合，计算时跳过两端都在其中的对。
        covered: HashSet<String>,
        /// 特征齐全、参与比对的成员图。
        images: Vec<SiftableImage>,
        /// 之前是否完成过整轮：区分首扫与补算的提示文案。
        had_coverage: bool,
    },
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

    /// 按哈希从本群所有库删图。只删索引行，不动文件：被删出索引的 blob
    /// 成为孤儿，由每日对账在备份保护期之外统一回收，误删的保护期内可恢复。
    pub async fn delete_hash(&self, group_id: i64, hash: &str) -> Result<Vec<String>, StoreError> {
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
            if !hash_still_used(&pool, hash).await? {
                delete_fingerprint(&pool, hash).await?;
                delete_sift(&pool, hash).await?;
                // 配对缓存的账也随哈希作废：正结果对回收，引用它的覆盖标记
                // 整行清除，防止同内容图再加回时对被误当已比对而漏检。
                delete_crop_pairs_for_hash(&pool, hash).await?;
                invalidate_crop_covered(&pool, hash).await?;
            }
            Ok(libraries)
        })
        .await
    }

    /// 清空整库前的确认信息。bytes 口径与「图库」列表一致（SUM(size)，共享图在多个库各计一次）。
    pub async fn library_overview(
        &self,
        group_id: i64,
        name: &str,
    ) -> Result<LibraryOverview, StoreError> {
        self.with_group(group_id, |pool| async move {
            let canonical = resolve_library(&pool, name).await?;
            let row = sqlx::query(
                "SELECT COUNT(*) AS count, SUM(size) AS bytes FROM images WHERE library = ?",
            )
            .bind(&canonical)
            .fetch_one(&pool)
            .await?;
            let count = row.try_get::<i64, _>("count")? as usize;
            if count == 0 {
                return Err(StoreError::LibraryMissing);
            }
            let bytes = row.try_get::<Option<i64>, _>("bytes")?.unwrap_or(0) as u64;
            Ok(LibraryOverview {
                canonical,
                count,
                bytes,
            })
        })
        .await
    }

    /// 二次确认的执行路径：清空登记时解析好的规范库名，跳过别名解析。
    /// 否则登记到确认之间，任何人用「别名 合并」都能把该名字改指别的库，
    /// 让确认清掉提示里没展示的那个库。
    pub async fn wipe_canonical_library(
        &self,
        group_id: i64,
        canonical: &str,
    ) -> Result<String, StoreError> {
        self.with_group(group_id, |pool| async move {
            Self::wipe_resolved(&pool, canonical).await
        })
        .await
    }

    /// 清空一个已解析到规范名的库：只删独占指纹、行和别名，blob 文件
    /// 留给对账按备份保护期回收。
    async fn wipe_resolved(pool: &SqlitePool, canonical: &str) -> Result<String, StoreError> {
        if !library_exists(pool, canonical).await? {
            return Err(StoreError::LibraryMissing);
        }
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
        .bind(canonical)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM sift WHERE hash IN (
                 SELECT mine.hash FROM images AS mine
                 WHERE mine.library = ?
                   AND NOT EXISTS (
                       SELECT 1 FROM images AS other
                       WHERE other.hash = mine.hash AND other.library != mine.library
                   )
             )",
        )
        .bind(canonical)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM images WHERE library = ?")
            .bind(canonical)
            .execute(&mut *tx)
            .await?;
        // 覆盖标记随库清空作废；正结果对可能被其他库共享，留给对账按
        // 「哈希彻底出库」回收。
        delete_crop_state(&mut *tx, canonical).await?;
        sqlx::query("DELETE FROM aliases WHERE target = ? OR alias = ?")
            .bind(canonical)
            .bind(canonical)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(canonical.to_owned())
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
    /// 重算缺失指纹是读盘加解码的重活：锁内只做快照与写回，解码在群锁外
    /// 并行进行，首查重全库重算期间同群的抽图/加图不会被卡住。
    pub async fn fingerprints_for_library(
        &self,
        group_id: i64,
        name: &str,
    ) -> Result<(String, Vec<HashedImage>), StoreError> {
        let blobs = self.blobs_dir(group_id);
        let (library, hashes, mut fingerprints, missing) = self
            .with_group(group_id, |pool| async move {
                let library = resolve_library(&pool, name).await?;
                if !library_exists(&pool, &library).await? {
                    return Err(StoreError::LibraryMissing);
                }
                let hashes: Vec<String> =
                    library_hashes(&pool, &library).await?.into_iter().collect();
                let fingerprints = library_fingerprints(&pool, &library).await?;
                // 锁外算指纹期间可能有并发删除，算完的孤儿行由对账任务清理。
                let missing: Vec<(String, PathBuf)> = hashes
                    .iter()
                    .filter(|hash| !fingerprints.contains_key(*hash))
                    .filter_map(|hash| {
                        blob_file(&blobs, hash)
                            .ok()
                            .map(|path| (hash.clone(), path))
                    })
                    .collect();
                Ok((library, hashes, fingerprints, missing))
            })
            .await?;

        let computed = derive_missing(missing, fingerprint_bytes).await?;

        if !computed.is_empty() {
            let to_write = &computed;
            self.with_group(group_id, |pool| async move {
                insert_fingerprints(&pool, to_write).await
            })
            .await?;
        }
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
    }

    /// 按规范库名补齐指纹与 SIFT 特征：两张表共用一次解码
    /// （[`fingerprint_and_sift`]），任一缺失的图都会重算——存量库升级后
    /// 首次查裁剪等于全库重算一遍，与首查重同量级。锁外解码期间同群的
    /// 抽图/加图不会被卡住（同上）。[`crop_scan_prepare`] 在锁内判完缓存
    /// 覆盖才走到这里，规范名直传，避免两段锁之间别名被并发改指。
    async fn siftables_for_canonical(
        &self,
        group_id: i64,
        library: &str,
    ) -> Result<Vec<SiftableImage>, StoreError> {
        let blobs = self.blobs_dir(group_id);
        let (hashes, mut fingerprints, mut sifts, missing) = self
            .with_group(group_id, |pool| async move {
                let hashes: Vec<String> =
                    library_hashes(&pool, library).await?.into_iter().collect();
                let fingerprints = library_fingerprints(&pool, library).await?;
                let sifts = library_sifts(&pool, library).await?;
                let missing: Vec<(String, PathBuf)> = hashes
                    .iter()
                    .filter(|hash| !fingerprints.contains_key(*hash) || !sifts.contains_key(*hash))
                    .filter_map(|hash| {
                        blob_file(&blobs, hash)
                            .ok()
                            .map(|path| (hash.clone(), path))
                    })
                    .collect();
                Ok((hashes, fingerprints, sifts, missing))
            })
            .await?;

        let computed = derive_missing(missing, fingerprint_and_sift).await?;

        if !computed.is_empty() {
            let to_fingerprints: Vec<_> = computed
                .iter()
                .map(|(hash, (fingerprint, _))| (hash.clone(), *fingerprint))
                .collect();
            let to_sifts: Vec<_> = computed
                .iter()
                .map(|(hash, (_, sift))| (hash.clone(), sift.clone()))
                .collect();
            self.with_group(group_id, |pool| {
                let to_fingerprints = &to_fingerprints;
                let to_sifts = &to_sifts;
                async move {
                    insert_fingerprints(&pool, to_fingerprints).await?;
                    insert_sifts(&pool, to_sifts).await
                }
            })
            .await?;
            for (hash, (fingerprint, sift)) in computed {
                fingerprints.insert(hash.clone(), fingerprint);
                sifts.insert(hash, sift);
            }
        }

        let images = hashes
            .into_iter()
            .filter_map(|hash| {
                let fingerprint = fingerprints.remove(&hash)?;
                let sift = sifts.remove(&hash)?;
                Some(SiftableImage {
                    hash,
                    fingerprint,
                    sift,
                })
            })
            .collect();
        Ok(images)
    }

    /// 「查裁剪」的缓存判定：覆盖集（最近一次完整扫描的成员全集）包含全部
    /// 成员时，直接从正结果缓存拼出结果；否则给出增量计划，只补算有新
    /// 端点的对。blob 按内容寻址不可变，算过的对永远有效，覆盖集只会因
    /// 删图/换阈值作废，不作日常收缩。
    pub async fn crop_scan_prepare(
        &self,
        group_id: i64,
        name: &str,
    ) -> Result<(String, CropPlan), StoreError> {
        let (library, members, covered, cached) = self
            .with_group(group_id, |pool| async move {
                let library = resolve_library(&pool, name).await?;
                if !library_exists(&pool, &library).await? {
                    return Err(StoreError::LibraryMissing);
                }
                let mut members: Vec<String> =
                    library_hashes(&pool, &library).await?.into_iter().collect();
                members.sort();
                let covered: HashSet<String> =
                    crop_covered(&pool, &library).await?.into_iter().collect();
                if members.len() < 2 || members.iter().all(|hash| covered.contains(hash)) {
                    let pairs = library_crop_pairs(&pool, &library).await?;
                    let groups = assemble_crop_groups(
                        pairs
                            .into_iter()
                            .map(|(whole, part, percent)| crop_group(&whole, &part, percent))
                            .collect(),
                    );
                    return Ok((library, members, covered, Some(groups)));
                }
                Ok((library, members, covered, None))
            })
            .await?;
        if let Some(groups) = cached {
            return Ok((library, CropPlan::Complete(groups)));
        }
        let had_coverage = !covered.is_empty();
        let images = self.siftables_for_canonical(group_id, &library).await?;
        Ok((
            library.clone(),
            CropPlan::Incremental {
                library,
                members,
                covered,
                images,
                had_coverage,
            },
        ))
    }

    /// 执行 [`CropPlan`]：全命中直接返回；补算只跑两端任一不在覆盖集里的
    /// 对，新正结果落库、本轮成员写进覆盖标记，最后统一从缓存重建（含
    /// 此前部分扫描攒下的正结果），排序截断与全量路径同一条代码。
    pub async fn crop_scan_run(
        &self,
        group_id: i64,
        plan: CropPlan,
    ) -> Result<Vec<SimilarGroup>, StoreError> {
        let (library, members, covered, images) = match plan {
            CropPlan::Complete(groups) => return Ok(groups),
            CropPlan::Incremental {
                library,
                members,
                covered,
                images,
                ..
            } => (library, members, covered, images),
        };
        let duplicate = crate::config::static_config().duplicate_distance();
        // 两两特征匹配是纯 CPU 的 O(n²)，让出 async worker（同查重）。
        let found = kovi::tokio::task::spawn_blocking(move || {
            let covered: HashSet<&str> = covered.iter().map(String::as_str).collect();
            detect_crops(&images, duplicate, &covered)
        })
        .await
        .map_err(|e| StoreError::Other(anyhow::anyhow!("查裁剪计算线程失败: {e}")))?;
        let fresh: Vec<(String, String, u8)> = found
            .iter()
            .map(|group| match group.hashes.as_slice() {
                [whole, part] => (whole.clone(), part.clone(), group.percent),
                // detect_crops 产出的裁剪组固定两张：整体在前、局部在后。
                _ => unreachable!("裁剪组固定两张"),
            })
            .collect();
        self.with_group(group_id, |pool| async move {
            insert_crop_pairs(&pool, &fresh).await?;
            save_crop_covered(&pool, &library, &members).await?;
            let stored = library_crop_pairs(&pool, &library).await?;
            Ok(assemble_crop_groups(
                stored
                    .into_iter()
                    .map(|(whole, part, percent)| crop_group(&whole, &part, percent))
                    .collect(),
            ))
        })
        .await
    }

    /// 每日维护入口:同一天数先备份再对账。「今天」只取一次传给两者,
    /// 避免备份跨 UTC 午夜后对账多算一天,把还有目录护着的记录提前清掉。
    pub(crate) async fn run_daily_maintenance(&self) {
        let today = backup::today_utc_days();
        self.backup_daily_at(today).await;
        self.reconcile_all_at(today).await;
    }

    pub(crate) async fn reconcile_all_at(&self, today: i64) {
        let groups = match self.list_group_ids().await {
            Ok(groups) => groups,
            Err(error) => {
                tracing::error!("列举图库群目录失败: {error}");
                return;
            }
        };
        for group_id in groups {
            if let Err(error) = self.reconcile_group(group_id, today).await {
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

    /// 对账是 blob 文件唯一的物理删除点：删除命令只清索引行，孤儿文件
    /// 在这里按备份保护期决定去留——备份还指向的保留，其余回收。
    async fn reconcile_group(&self, group_id: i64, today: i64) -> Result<(), StoreError> {
        let blobs = self.blobs_dir(group_id);
        self.with_group(group_id, |pool| async move {
            purge_expired_backup_refs(&pool, backup::RETAINED_DAYS as i64, today).await?;
            let protected = backed_up_hashes(&pool).await?;

            let disk = blob_hashes_on_disk(&blobs).await?;

            let indexed: HashSet<String> = sqlx::query_scalar("SELECT DISTINCT hash FROM images")
                .fetch_all(&pool)
                .await?
                .into_iter()
                .collect();

            let mut removed_files = 0u64;
            for hash in disk.difference(&indexed) {
                if protected.contains(hash) {
                    continue;
                }
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
            sqlx::query("DELETE FROM sift WHERE hash NOT IN (SELECT DISTINCT hash FROM images)")
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "DELETE FROM crop_pairs
                 WHERE whole NOT IN (SELECT DISTINCT hash FROM images)
                    OR part NOT IN (SELECT DISTINCT hash FROM images)",
            )
            .execute(&mut *tx)
            .await?;
            // 覆盖标记引用了已出库哈希的库整行作废：正结果行刚被回收，
            // 留着标记会把「删掉再加回」的对误当已比对。库数量级小，
            // 在 Rust 侧逐行查 covered 是否全在库内即可。
            let live: HashSet<String> = indexed.intersection(&disk).cloned().collect();
            let state_rows = sqlx::query("SELECT library, covered FROM crop_scan_state")
                .fetch_all(&mut *tx)
                .await?;
            for row in state_rows {
                let library = row.try_get::<String, _>("library")?;
                let covered = row.try_get::<String, _>("covered")?;
                let stale = covered
                    .as_bytes()
                    .chunks(64)
                    .any(|chunk| std::str::from_utf8(chunk).is_ok_and(|hash| !live.contains(hash)));
                if stale {
                    delete_crop_state(&mut *tx, &library).await?;
                }
            }
            tx.commit().await?;

            if removed_files > 0 || removed_rows > 0 {
                tracing::info!(group_id, removed_files, removed_rows, "图库对账完成");
            }
            Ok(())
        })
        .await
    }
}

/// 解码后的像素缓冲（RGB + luma 副本按每像素 4 字节估算）超过此值的图
/// 不进并行队列，交给单独的串行队列：多张大缓冲同时解会把峰值内存叠到
/// 数倍，在无 swap、余量紧张的部署机上不可接受。文件字节是被压缩过的
/// 差代理（一张 4 MiB 的 JPEG 也能解出 6000×8000），分道看解码后大小。
const PARALLEL_DECODE_LIMIT: u64 = 32 * 1024 * 1024;

/// 头部预读字节数：PNG IHDR 在前 33 字节，JPEG 的 SOF 通常也在头部；
/// 带 Exif 缩略图的 JPEG 段可能更长，解析不出时退回文件字节兜底。
const DIMENSION_HEAD: usize = 64 * 1024;

/// 读文件头估出解码后大小，决定走并行还是串行队列。
/// blob 按内容寻址、写入后不变，头部足以定案。
fn is_large_blob(path: &PathBuf) -> bool {
    let mut head = vec![0u8; DIMENSION_HEAD];
    let read =
        std::fs::File::open(path).and_then(|mut file| std::io::Read::read(&mut file, &mut head));
    match read {
        Ok(n) => {
            head.truncate(n);
            match crate::similar::pixel_dimensions(&head) {
                Some((w, h)) => u64::from(w) * u64::from(h) * 4 > PARALLEL_DECODE_LIMIT,
                // 头解析不出：退回文件字节判据，宁可串行也不放进并行。
                None => std::fs::metadata(path).is_ok_and(|meta| meta.len() > 4 * 1024 * 1024),
            }
        }
        Err(_) => true,
    }
}

/// 单个解码 worker：固定数量的 async 任务，从队列动态领活，同一时刻
/// 只挂一个 blocking 解码，所以占用的解码线程数恒等于 worker 数。
/// 队列关闭（发送端全部 drop）且排空后 `recv` 返回 Err，worker 自然退出。
fn spawn_derive_worker<T, F>(
    rx: async_channel::Receiver<(String, Vec<u8>)>,
    compute: F,
) -> kovi::tokio::task::JoinHandle<anyhow::Result<Vec<(String, T)>>>
where
    T: Send + 'static,
    F: Fn(&[u8]) -> Option<T> + Send + Sync + Clone + 'static,
{
    kovi::tokio::spawn(async move {
        let mut computed = Vec::new();
        while let Ok((hash, bytes)) = rx.recv().await {
            let derived = kovi::tokio::task::spawn_blocking({
                let compute = compute.clone();
                move || compute(&bytes)
            })
            .await
            .map_err(|e| anyhow::anyhow!("计算派生特征失败: {e}"))?;
            if let Some(derived) = derived {
                computed.push((hash, derived));
            }
        }
        Ok(computed)
    })
}

/// 读盘走 async，解码占 blocking 线程。队列是 async-channel（MPMC，
/// Receiver 可 Clone）：一条小图队列 workers 个 worker 动态领活、谁快
/// 谁多拿；一条大图队列单 worker，同一时刻最多一张大图在解码。两条
/// 队列同为容量 1：在途水位 = 每队列一张排队 + worker 在手的各一张。
/// 大图的读盘与发送单独成一个任务——大图读得慢、大图队列又被慢解码
/// 顶住背压，混在一个发送循环里会周期性断掉小图的供给。
async fn derive_missing<T, F>(
    missing: Vec<(String, PathBuf)>,
    compute: F,
) -> Result<Vec<(String, T)>, StoreError>
where
    T: Send + 'static,
    F: Fn(&[u8]) -> Option<T> + Send + Sync + Clone + 'static,
{
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    // blob 按内容寻址、写入后不变，读头部即可在载入前完成分道。
    let mut small = Vec::with_capacity(missing.len());
    let mut large = Vec::new();
    for entry in missing {
        if is_large_blob(&entry.1) {
            large.push(entry);
        } else {
            small.push(entry);
        }
    }

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .min(4);
    let (small_tx, small_rx) = async_channel::bounded::<(String, Vec<u8>)>(1);
    let (large_tx, large_rx) = async_channel::bounded::<(String, Vec<u8>)>(1);
    let mut handles = Vec::with_capacity(workers + 1);
    for _ in 0..workers {
        handles.push(spawn_derive_worker(small_rx.clone(), compute.clone()));
    }
    handles.push(spawn_derive_worker(large_rx, compute));

    let large_sender = kovi::tokio::spawn(async move {
        for (hash, path) in large {
            let Ok(bytes) = kovi::tokio::fs::read(path).await else {
                continue;
            };
            // worker 崩溃才 send 失败，此时统一由下面的 JoinHandle 报错。
            let _ = large_tx.send((hash, bytes)).await;
        }
        // large_tx 随任务结束 drop，大图 worker 排空后自然退出。
    });

    for (hash, path) in small {
        let Ok(bytes) = kovi::tokio::fs::read(path).await else {
            continue;
        };
        let _ = small_tx.send((hash, bytes)).await;
    }
    drop(small_tx);

    large_sender
        .await
        .map_err(|e| StoreError::Other(anyhow::anyhow!("发送大图指纹任务失败: {e}")))?;
    let mut all = Vec::new();
    for handle in handles {
        let computed = handle
            .await
            .map_err(|e| StoreError::Other(anyhow::anyhow!("计算派生特征失败: {e}")))??;
        all.extend(computed);
    }
    Ok(all)
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
