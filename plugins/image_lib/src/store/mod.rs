//! 按群分片的图库存储：`<root>/<group_id>/index.db` 存元数据，
//! `blobs/<sha256>` 存内容寻址的图片文件。
//! SQL 在 [`repo`]，文件层在 [`blob_fs`]，建表迁移在 [`schema`]，派生
//! 特征解码在 [`derive`]，查重与查裁剪的读路径在 [`scan`]。

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use kovi::tokio::sync::Mutex;

use anyhow::{Context, Result};
use rand::seq::IndexedRandom;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::similar::{SiftableImage, SimilarGroup};

mod backup;
mod blob_fs;
mod cloud_sync;
mod derive;
mod repo;
mod scan;
mod schema;

#[cfg(test)]
mod tests;

use blob_fs::{blob_file, blob_hashes_on_disk, is_hash_prefix, promote_staged, remove_unindexed};
use repo::{
    additional_unique_bytes, alias_pairs, alias_target, backed_up_hashes, bump_draw_count,
    delete_alias, delete_covered_for_hash, delete_crop_pairs_for_hash, delete_derived_exclusive_to,
    delete_fingerprint, delete_image_rows, delete_sift, hash_still_used, hashes_by_prefix,
    indexed_hashes, insert_images, library_draw_items, library_exists, library_hashes,
    library_size, library_size_rows, merge_library, prune_dangling_aliases,
    prune_derived_not_referenced, purge_expired_backup_refs, resolve_library, unique_image_bytes,
    unique_image_count, upsert_alias, wipe_library_rows,
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
    /// 备份保留天数，目录滚动与对账保护期必须用同一个值。
    retained_days: usize,
    /// 群锁和连接池都在 async 路径上取，用 tokio Mutex 以免卡住 runtime。
    locks: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
    pools: Mutex<HashMap<i64, SqlitePool>>,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        let config = crate::config::static_config();
        Self::open_with_quota(root, config.max_group_bytes(), config.backup_retained_days)
    }

    pub(crate) fn open_with_quota(
        root: PathBuf,
        max_group_bytes: u64,
        retained_days: usize,
    ) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建图库目录失败: {}", root.display()))?;
        Ok(Self {
            root,
            max_group_bytes,
            retained_days: retained_days.max(1),
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

    pub(crate) fn blob_path(&self, group_id: i64, hash: &str) -> Result<PathBuf> {
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
            let mut libraries = delete_image_rows(&pool, hash).await?;
            if libraries.is_empty() {
                return Err(StoreError::ImageMissing);
            }
            libraries.sort();
            libraries.dedup();
            prune_dangling_aliases(&pool).await?;
            if !hash_still_used(&pool, hash).await? {
                delete_fingerprint(&pool, hash).await?;
                delete_sift(&pool, hash).await?;
                // 配对缓存不随逻辑删除动：正结果对与覆盖标记保留，删图后
                // 重查直接命中（被删哈希由结果重建的 EXISTS 过滤），同内容
                // 图在 blob 保护期内加回也零成本。作废绑定在磁盘物理回收
                // 上，见 reconcile_group。
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
            let (count, bytes) = library_size(&pool, &canonical).await?;
            if count == 0 {
                return Err(StoreError::LibraryMissing);
            }
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
        delete_derived_exclusive_to(&mut tx, canonical).await?;
        // 覆盖标记与正结果对都保留：库清空后标记惰性无害（成员为空），
        // 用同批图重建时依然全命中；blob 被对账回收时才收缩。
        wipe_library_rows(&mut tx, canonical).await?;
        tx.commit().await?;
        Ok(canonical.to_owned())
    }

    pub async fn pick_random(&self, group_id: i64, name: &str) -> Result<String, StoreError> {
        self.with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            let items = library_draw_items(&pool, &library).await?;
            let hash = pick_weighted(&items)
                .map(str::to_owned)
                .ok_or(StoreError::LibraryEmpty)?;
            bump_draw_count(&pool, &library, &hash).await?;
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
            let existing_target = alias_target(&pool, alias).await?;

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
                upsert_alias(&pool, alias, &canonical).await?;
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
            if delete_alias(&pool, alias).await? == 0 {
                return Err(StoreError::AliasMissing(alias.to_owned()));
            }
            Ok(())
        })
        .await
    }

    pub async fn stats(&self, group_id: i64) -> Result<GroupStats, StoreError> {
        self.with_group(group_id, |pool| async move {
            let rows = library_size_rows(&pool).await?;
            let mut aliases: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (alias, target) in alias_pairs(&pool).await? {
                aliases.entry(target).or_default().push(alias);
            }
            let libraries = rows
                .into_iter()
                .map(|(name, count, bytes)| LibraryStat {
                    aliases: aliases.remove(&name).unwrap_or_default(),
                    name,
                    count: count as usize,
                    bytes: bytes as u64,
                })
                .collect();
            let unique_count = unique_image_count(&pool).await?;
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
            let hashes = hashes_by_prefix(&pool, &pattern).await?;
            match hashes.as_slice() {
                [hash] => Ok(hash.clone()),
                [] => Err(StoreError::ImageMissing),
                _ => Err(StoreError::HashAmbiguous),
            }
        })
        .await
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

    /// 每日维护入口:同一天数先备份再对账。「今天」只取一次传给两者,
    /// 避免备份跨 UTC 午夜后对账多算一天,把还有目录护着的记录提前清掉。
    pub(crate) async fn run_daily_maintenance(&self, cloud_sync: Option<&str>) {
        let today = backup::today_utc_days();
        self.backup_daily_at(today).await;
        // 云同步夹在备份与对账之间:先让云端拿到全部快照要引用的文件,
        // 再允许对账回收过期孤儿。
        if let Some(destination) = cloud_sync {
            self.cloud_sync(destination).await;
        }
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
            purge_expired_backup_refs(&pool, self.retained_days as i64, today).await?;
            let protected = backed_up_hashes(&pool).await?;

            let disk = blob_hashes_on_disk(&blobs).await?;

            let indexed = indexed_hashes(&pool).await?;

            let mut removed_files = 0u64;
            // blob 物理消失的哈希（这里删掉孤儿文件，或下面索引指向但盘上
            // 已没有）：配对缓存的正结果对与覆盖行同批作废，此后同内容
            // 图再加回按增量只重算它的对。逻辑删图不动缓存（见 delete_hash）。
            let mut reclaimed: HashSet<String> = HashSet::new();
            for hash in disk.difference(&indexed) {
                if protected.contains(hash) {
                    continue;
                }
                if let Ok(path) = blob_file(&blobs, hash)
                    && kovi::tokio::fs::remove_file(&path).await.is_ok()
                {
                    removed_files += 1;
                    reclaimed.insert(hash.clone());
                }
            }

            let mut tx = pool.begin().await?;
            let mut removed_rows = 0u64;
            for hash in indexed.difference(&disk) {
                reclaimed.insert(hash.clone());
                removed_rows += delete_image_rows(&mut *tx, hash).await?.len() as u64;
            }
            if removed_rows > 0 {
                prune_dangling_aliases(&mut *tx).await?;
            }
            prune_derived_not_referenced(&mut tx).await?;
            for hash in &reclaimed {
                delete_crop_pairs_for_hash(&mut *tx, hash).await?;
                delete_covered_for_hash(&mut *tx, hash).await?;
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
