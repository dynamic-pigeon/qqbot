use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use kovi::tokio::sync::{Mutex, mpsc};

use anyhow::{Context, Result};
use rand::RngExt;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::similar::{Fingerprint, HashedImage, fingerprint_bytes};
use utils::sha256_hex;

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
    #[error("「{0}」已是图库，不能当别名")]
    NameIsLibrary(String),
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

    fn blobs_dir(&self, group_id: i64) -> PathBuf {
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
        images: Vec<Vec<u8>>,
    ) -> Result<AddResult, StoreError> {
        let blobs = self.blobs_dir(group_id);
        let max_group_bytes = self.max_group_bytes;
        self.with_group(group_id, |pool| {
            let images = &images;
            async move {
                kovi::tokio::fs::create_dir_all(&blobs)
                    .await
                    .context("创建图片目录失败")?;
                let library = resolve_library(&pool, name).await?;
                let existing = library_hashes(&pool, &library).await?;

                let mut added_hashes = HashSet::new();
                let mut to_insert = Vec::new();
                let mut skipped_dup = 0usize;

                for bytes in images {
                    if bytes.is_empty() {
                        return Err(StoreError::Other(anyhow::anyhow!("图片为空")));
                    }
                    let hash = sha256_hex(bytes);
                    if existing.contains(&hash) || !added_hashes.insert(hash.clone()) {
                        skipped_dup += 1;
                        continue;
                    }
                    to_insert.push((hash, bytes.as_slice()));
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
                for (hash, bytes) in &to_insert {
                    let path = blob_file(&blobs, hash)?;
                    if kovi::tokio::fs::try_exists(&path).await.unwrap_or(false) {
                        continue;
                    }
                    if let Err(error) = write_blob_atomic(&path, bytes).await {
                        let _ = remove_unindexed(&pool, &created).await;
                        return Err(error.into());
                    }
                    created.push((hash.clone(), path));
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
        .await
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
            let hash = pick_weighted(&items, &mut rand::rng())
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
    ) -> Result<String, StoreError> {
        self.with_group(group_id, |pool| async move {
            let canonical = resolve_library(&pool, target).await?;
            if alias == canonical {
                return Err(StoreError::AliasToSelf);
            }
            if !library_exists(&pool, &canonical).await? {
                return Err(StoreError::TargetMissing(target.to_owned()));
            }
            if library_exists(&pool, alias).await? {
                return Err(StoreError::NameIsLibrary(alias.to_owned()));
            }
            sqlx::query(
                "INSERT INTO aliases (alias, target) VALUES (?, ?)
                 ON CONFLICT(alias) DO UPDATE SET target = excluded.target",
            )
            .bind(alias)
            .bind(&canonical)
            .execute(&pool)
            .await?;
            Ok(canonical)
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

async fn init_schema(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS images (
            library TEXT NOT NULL,
            hash TEXT NOT NULL CHECK (length(hash) = 64),
            size INTEGER NOT NULL CHECK (size > 0),
            draw_count INTEGER NOT NULL DEFAULT 0 CHECK (draw_count >= 0),
            PRIMARY KEY (library, hash)
        )",
    )
    .execute(pool)
    .await?;
    ensure_draw_count_column(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS aliases (
            alias TEXT NOT NULL PRIMARY KEY,
            target TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_images_hash ON images(hash)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_aliases_target ON aliases(target)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS perceptual (
            hash TEXT NOT NULL PRIMARY KEY CHECK (length(hash) = 64),
            dhash INTEGER NOT NULL,
            phash INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_draw_count_column(pool: &SqlitePool) -> Result<(), StoreError> {
    let rows = sqlx::query("PRAGMA table_info(images)")
        .fetch_all(pool)
        .await?;
    let exists = rows.iter().any(|row| {
        row.try_get::<String, _>("name")
            .is_ok_and(|name| name == "draw_count")
    });
    if !exists {
        sqlx::query("ALTER TABLE images ADD COLUMN draw_count INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// 权重 `4096 >> (次数 - 库内最小次数)`，最少的那档是 4096，最多落后 12 次仍为 1。
fn pick_weighted<'a>(items: &'a [(String, i64)], rng: &mut impl RngExt) -> Option<&'a str> {
    let min = items.iter().map(|(_, count)| *count).min()?;
    let mut total: u32 = 0;
    for (_, count) in items {
        total += weight(*count, min);
    }
    let mut throw = rng.random_range(0..total);
    for (hash, count) in items {
        let w = weight(*count, min);
        if throw < w {
            return Some(hash);
        }
        throw -= w;
    }
    items.last().map(|(hash, _)| hash.as_str())
}

fn weight(count: i64, min: i64) -> u32 {
    const MAX_WEIGHT: u32 = 1 << 12;
    MAX_WEIGHT >> (count.saturating_sub(min) as u32).min(12)
}

async fn resolve_library(pool: &SqlitePool, name: &str) -> Result<String, StoreError> {
    let target = sqlx::query_scalar::<_, String>("SELECT target FROM aliases WHERE alias = ?")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(target.unwrap_or_else(|| name.to_owned()))
}

async fn library_exists(pool: &SqlitePool, name: &str) -> Result<bool, StoreError> {
    let found = sqlx::query_scalar::<_, i64>("SELECT 1 FROM images WHERE library = ? LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

async fn library_hashes(pool: &SqlitePool, library: &str) -> Result<HashSet<String>, StoreError> {
    let hashes = sqlx::query_scalar::<_, String>("SELECT hash FROM images WHERE library = ?")
        .bind(library)
        .fetch_all(pool)
        .await?;
    Ok(hashes.into_iter().collect())
}

async fn unique_image_bytes(pool: &SqlitePool) -> Result<u64, StoreError> {
    let bytes = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT SUM(size) FROM (SELECT hash, MAX(size) AS size FROM images GROUP BY hash)",
    )
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    Ok(bytes as u64)
}

async fn additional_unique_bytes(
    pool: &SqlitePool,
    to_insert: &[(String, &[u8])],
) -> Result<u64, StoreError> {
    if to_insert.is_empty() {
        return Ok(0);
    }
    let mut builder =
        sqlx::QueryBuilder::<sqlx::Sqlite>::new("SELECT DISTINCT hash FROM images WHERE hash IN (");
    {
        let mut separated = builder.separated(", ");
        for (hash, _) in to_insert {
            separated.push_bind(hash);
        }
    }
    builder.push(")");
    let present: HashSet<String> = builder
        .build_query_scalar()
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect();
    Ok(to_insert
        .iter()
        .filter(|(hash, _)| !present.contains(hash))
        .map(|(_, bytes)| bytes.len() as u64)
        .sum())
}

async fn hashes_only_in_library(
    pool: &SqlitePool,
    library: &str,
) -> Result<Vec<String>, StoreError> {
    let hashes = sqlx::query_scalar::<_, String>(
        "SELECT mine.hash FROM images AS mine
         WHERE mine.library = ?
           AND NOT EXISTS (
               SELECT 1 FROM images AS other
               WHERE other.hash = mine.hash AND other.library != mine.library
           )",
    )
    .bind(library)
    .fetch_all(pool)
    .await?;
    Ok(hashes)
}

async fn hash_still_used(pool: &SqlitePool, hash: &str) -> Result<bool, StoreError> {
    let found = sqlx::query_scalar::<_, i64>("SELECT 1 FROM images WHERE hash = ? LIMIT 1")
        .bind(hash)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

async fn insert_fingerprints(
    pool: &SqlitePool,
    fingerprints: &[(String, Fingerprint)],
) -> Result<(), StoreError> {
    for (hash, fingerprint) in fingerprints {
        sqlx::query("INSERT OR IGNORE INTO perceptual (hash, dhash, phash) VALUES (?, ?, ?)")
            .bind(hash)
            .bind(fingerprint.dhash as i64)
            .bind(fingerprint.phash as i64)
            .execute(pool)
            .await?;
    }
    Ok(())
}

async fn delete_fingerprint(pool: &SqlitePool, hash: &str) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM perceptual WHERE hash = ?")
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

async fn library_fingerprints(
    pool: &SqlitePool,
    library: &str,
) -> Result<HashMap<String, Fingerprint>, StoreError> {
    let rows = sqlx::query(
        "SELECT p.hash AS hash, p.dhash AS dhash, p.phash AS phash
         FROM perceptual p
         INNER JOIN images i ON i.hash = p.hash
         WHERE i.library = ?",
    )
    .bind(library)
    .fetch_all(pool)
    .await?;
    let mut found = HashMap::new();
    for row in rows {
        found.insert(
            row.try_get::<String, _>("hash")?,
            Fingerprint {
                dhash: row.try_get::<i64, _>("dhash")? as u64,
                phash: row.try_get::<i64, _>("phash")? as u64,
            },
        );
    }
    Ok(found)
}

async fn insert_images(
    pool: &SqlitePool,
    library: &str,
    images: &[(String, &[u8])],
) -> Result<(), StoreError> {
    let draw_count = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(draw_count) FROM images WHERE library = ?",
    )
    .bind(library)
    .fetch_one(pool)
    .await?
    .unwrap_or(0);
    for (hash, bytes) in images {
        sqlx::query(
            "INSERT OR IGNORE INTO images (library, hash, size, draw_count) VALUES (?, ?, ?, ?)",
        )
        .bind(library)
        .bind(hash)
        .bind(bytes.len() as i64)
        .bind(draw_count)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn prune_dangling_aliases(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM aliases WHERE target NOT IN (SELECT DISTINCT library FROM images)")
        .execute(pool)
        .await?;
    Ok(())
}

fn blob_file(blobs: &Path, hash: &str) -> Result<PathBuf> {
    if !is_blob_hash(hash) {
        anyhow::bail!("非法图片哈希");
    }
    Ok(blobs.join(hash))
}

async fn blob_hashes_on_disk(blobs: &Path) -> Result<HashSet<String>, StoreError> {
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

fn is_hash_prefix(prefix: &str) -> bool {
    let len = prefix.len();
    (1..=64).contains(&len) && prefix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn write_blob_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(
        "{name}.{}.{:x}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let write = async {
        kovi::tokio::fs::write(&tmp, bytes).await?;
        utils::restrict_mode_0600(&tmp)?;
        kovi::tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
    .await;
    if write.is_err() {
        let _ = kovi::tokio::fs::remove_file(&tmp).await;
    }
    write
}

async fn remove_unindexed(
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

#[cfg(test)]
mod tests {
    use super::*;
    use kovi::tokio;

    fn temp_store() -> (Store, PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("image_lib_store_{}_{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (
            Store::open_with_quota(
                dir.clone(),
                crate::config::DEFAULT_MAX_GROUP_MIB * 1024 * 1024,
            )
            .unwrap(),
            dir,
        )
    }

    fn png_like(tag: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG".to_vec();
        bytes.extend_from_slice(&[tag; 32]);
        bytes
    }

    async fn add_images(
        store: &Store,
        group_id: i64,
        name: &str,
        images: Vec<Vec<u8>>,
    ) -> Result<AddResult, StoreError> {
        store.add_images(group_id, name, images).await
    }

    #[tokio::test]
    async fn adds_dedups_shares_blob_and_deletes() {
        let (store, dir) = temp_store();
        let group = 1;
        let a = png_like(1);
        let b = png_like(2);

        let first = add_images(&store, group, "猫", vec![a.clone()])
            .await
            .unwrap();
        assert_eq!(
            first,
            AddResult {
                added: 1,
                skipped_dup: 0
            }
        );
        let again = add_images(&store, group, "猫", vec![a.clone()])
            .await
            .unwrap();
        assert_eq!(
            again,
            AddResult {
                added: 0,
                skipped_dup: 1
            }
        );
        add_images(&store, group, "狗", vec![a.clone(), b.clone()])
            .await
            .unwrap();

        let blobs = std::fs::read_dir(dir.join("1").join("blobs"))
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .ok()
                    .is_some_and(|e| e.path().extension().is_none())
            })
            .count();
        assert_eq!(blobs, 2);

        let hit = store.delete_hash(group, &sha256_hex(&a)).await.unwrap();
        let mut hit = hit;
        hit.sort();
        assert_eq!(hit, vec!["狗".to_owned(), "猫".to_owned()]);
        assert!(matches!(
            store.pick_random(group, "猫").await,
            Err(StoreError::LibraryMissing | StoreError::LibraryEmpty)
        ));
        assert!(store.pick_random(group, "狗").await.is_ok());
        assert_eq!(store.stats(group).await.unwrap().libraries.len(), 1);

        store.wipe_library(group, "狗").await.unwrap();
        assert!(store.stats(group).await.unwrap().libraries.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn alias_resolves_rejects_and_wipe_clears_canonical() {
        let (store, dir) = temp_store();
        let group = 2;
        add_images(&store, group, "猫", vec![png_like(1)])
            .await
            .unwrap();
        add_images(&store, group, "狗", vec![png_like(3)])
            .await
            .unwrap();
        let canonical = store.set_alias(group, "喵", "猫").await.unwrap();
        assert_eq!(canonical, "猫");
        assert!(store.pick_random(group, "喵").await.is_ok());
        assert!(matches!(
            store.set_alias(group, "狗", "猫").await,
            Err(StoreError::NameIsLibrary(_))
        ));
        assert!(matches!(
            store.set_alias(group, "龙", "不存在").await,
            Err(StoreError::TargetMissing(_))
        ));
        assert!(matches!(
            store.set_alias(group, "猫", "猫").await,
            Err(StoreError::AliasToSelf)
        ));

        add_images(&store, group, "喵", vec![png_like(2)])
            .await
            .unwrap();
        let stats = store.stats(group).await.unwrap();
        assert_eq!(stats.libraries.len(), 2);
        let cat = stats
            .libraries
            .iter()
            .find(|library| library.name == "猫")
            .unwrap();
        assert_eq!(cat.aliases, vec!["喵".to_owned()]);
        assert_eq!(cat.count, 2);

        let wiped = store.wipe_library(group, "喵").await.unwrap();
        assert_eq!(wiped, "猫");
        assert_eq!(store.stats(group).await.unwrap().libraries.len(), 1);
        assert!(matches!(
            store.pick_random(group, "喵").await,
            Err(StoreError::LibraryEmpty)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rejects_when_group_quota_would_exceed() {
        let dir = std::env::temp_dir().join(format!("image_lib_quota_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open_with_quota(dir.clone(), 40).unwrap();
        add_images(&store, 7, "小", vec![png_like(3)])
            .await
            .unwrap();
        let err = add_images(&store, 7, "小", vec![png_like(4)])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::QuotaExceeded { .. }));

        let err = add_images(&store, 8, "小", vec![png_like(3), png_like(4)])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::QuotaExceeded { .. }));
        let leftover = std::fs::read_dir(dir.join("8").join("blobs"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_none())
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(leftover, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn patterned_png(seed: u32) -> Vec<u8> {
        use image::{DynamicImage, Rgb, RgbImage};
        let image = RgbImage::from_fn(48, 48, |x, y| {
            let v = ((x.wrapping_mul(11) + y.wrapping_mul(5) + seed) % 256) as u8;
            Rgb([v, v.wrapping_add(30), 200u8.wrapping_sub(v)])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[tokio::test]
    async fn fingerprints_cover_library_and_skip_undecodable() {
        let (store, dir) = temp_store();
        let group = 11;
        add_images(
            &store,
            group,
            "猫",
            vec![patterned_png(1), patterned_png(2)],
        )
        .await
        .unwrap();
        add_images(&store, group, "猫", vec![png_like(9)])
            .await
            .unwrap();

        let (canonical, images) = store.fingerprints_for_library(group, "猫").await.unwrap();
        assert_eq!(canonical, "猫");
        assert_eq!(images.len(), 2);

        store.set_alias(group, "喵", "猫").await.unwrap();
        let (alias, again) = store.fingerprints_for_library(group, "喵").await.unwrap();
        assert_eq!(alias, "猫");
        assert_eq!(again.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn hash_prefix_loads_deletes_and_rejects_bad_paths() {
        let (store, dir) = temp_store();
        let group = 21;
        let a = png_like(1);
        let b = png_like(2);
        add_images(&store, group, "猫", vec![a.clone(), b.clone()])
            .await
            .unwrap();
        add_images(&store, group, "狗", vec![a.clone()])
            .await
            .unwrap();

        let ha = sha256_hex(&a);
        let hb = sha256_hex(&b);
        assert_eq!(store.load_by_hash_prefix(group, &ha).await.unwrap(), a);

        let prefix = &ha[..12];
        assert!(
            !hb.starts_with(prefix),
            "fixture blobs unexpectedly share a 12-hex prefix"
        );
        assert_eq!(store.load_by_hash_prefix(group, prefix).await.unwrap(), a);
        assert_eq!(
            store
                .load_by_hash_prefix(group, &prefix.to_ascii_uppercase())
                .await
                .unwrap(),
            a
        );
        assert!(matches!(
            store
                .load_by_hash_prefix(group, "ffffffffffffffffffffffffffffffff")
                .await,
            Err(StoreError::ImageMissing)
        ));

        let libraries = store.delete_by_hash_prefix(group, prefix).await.unwrap();
        assert_eq!(libraries, vec!["狗".to_owned(), "猫".to_owned()]);
        assert!(matches!(
            store.load_by_hash_prefix(group, &ha).await,
            Err(StoreError::ImageMissing)
        ));
        assert_eq!(
            store.load_by_hash_prefix(group, &hb[..12]).await.unwrap(),
            b
        );

        assert!(store.read_blob(group, "../passwd").await.is_err());
        assert!(store.read_blob(group, "zz").await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn draw_counts(
        store: &Store,
        group_id: i64,
        name: &str,
    ) -> Result<Vec<(String, i64)>, StoreError> {
        store
            .with_group(group_id, |pool| async move {
                let library = resolve_library(&pool, name).await?;
                let rows = sqlx::query(
                    "SELECT hash, draw_count FROM images WHERE library = ? ORDER BY hash",
                )
                .bind(&library)
                .fetch_all(&pool)
                .await?;
                rows.into_iter()
                    .map(|row| {
                        Ok((
                            row.try_get::<String, _>("hash")?,
                            row.try_get::<i64, _>("draw_count")?,
                        ))
                    })
                    .collect()
            })
            .await
    }

    #[tokio::test]
    async fn new_images_start_at_library_min_and_pick_increments() {
        let (store, dir) = temp_store();
        let group = 31;
        let a = png_like(1);
        let b = png_like(2);
        add_images(&store, group, "猫", vec![a.clone()])
            .await
            .unwrap();
        store.pick_random(group, "猫").await.unwrap();
        add_images(&store, group, "猫", vec![b.clone()])
            .await
            .unwrap();
        let mut expected = vec![(sha256_hex(&a), 1), (sha256_hex(&b), 1)];
        expected.sort();
        assert_eq!(draw_counts(&store, group, "猫").await.unwrap(), expected);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn weight_halves_per_extra_draw() {
        let min = 3;
        assert_eq!(weight(min, min), 1 << 12);
        assert_eq!(weight(min + 1, min), 1 << 11);
        assert_eq!(weight(min + 12, min), 1);
        assert_eq!(weight(min + 13, min), 1);
    }
}
