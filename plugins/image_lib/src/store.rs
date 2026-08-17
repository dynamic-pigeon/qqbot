use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

pub const MAX_GROUP_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("本群图库容量不足")]
    QuotaExceeded { used: u64, additional: u64 },
    #[error("库不存在")]
    LibraryMissing,
    #[error("没有这张图")]
    ImageMissing,
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
    locks: Mutex<HashMap<i64, Arc<kovi::tokio::sync::Mutex<()>>>>,
    pools: Mutex<HashMap<i64, SqlitePool>>,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        Self::open_with_quota(root, MAX_GROUP_BYTES)
    }

    fn open_with_quota(root: PathBuf, max_group_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("创建图库目录失败: {}", root.display()))?;
        Ok(Self {
            root,
            max_group_bytes,
            locks: Mutex::new(HashMap::new()),
            pools: Mutex::new(HashMap::new()),
        })
    }

    fn group_lock(&self, group_id: i64) -> Arc<kovi::tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(group_id)
            .or_insert_with(|| Arc::new(kovi::tokio::sync::Mutex::new(())))
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
        if !is_blob_hash(hash) {
            anyhow::bail!("非法图片哈希");
        }
        Ok(self.blobs_dir(group_id).join(hash))
    }

    async fn with_group<T, F, Fut>(&self, group_id: i64, f: F) -> Result<T, StoreError>
    where
        F: FnOnce(SqlitePool) -> Fut,
        Fut: Future<Output = Result<T, StoreError>>,
    {
        let lock = self.group_lock(group_id);
        let _guard = lock.lock().await;
        let pool = self.ensure_pool(group_id).await?;
        f(pool).await
    }

    async fn ensure_pool(&self, group_id: i64) -> Result<SqlitePool, StoreError> {
        if let Some(pool) = self
            .pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&group_id)
        {
            return Ok(pool.clone());
        }

        let dir = self.group_dir(group_id);
        std::fs::create_dir_all(&dir).context("创建群图库目录失败")?;
        std::fs::create_dir_all(self.blobs_dir(group_id)).context("创建图片目录失败")?;
        let db_path = self.db_path(group_id);
        let options = SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        init_schema(&pool).await?;
        restrict_file_permissions(&db_path)?;
        self.pools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(group_id, pool.clone());
        Ok(pool)
    }

    pub async fn add_images(
        &self,
        group_id: i64,
        name: &str,
        images: Vec<Vec<u8>>,
    ) -> Result<AddResult, StoreError> {
        let prepared: Vec<(String, u64, Vec<u8>)> = images
            .into_iter()
            .map(|bytes| {
                let hash = sha256_hex(&bytes);
                let size = bytes.len() as u64;
                (hash, size, bytes)
            })
            .collect();

        let blobs = self.blobs_dir(group_id);
        let max_group_bytes = self.max_group_bytes;
        self.with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            let existing = library_hashes(&pool, &library).await?;

            let mut added_hashes = HashSet::new();
            let mut to_insert = Vec::new();
            let mut skipped_dup = 0usize;
            for (hash, size, bytes) in prepared {
                if existing.contains(&hash) || !added_hashes.insert(hash.clone()) {
                    skipped_dup += 1;
                    continue;
                }
                to_insert.push((hash, size, bytes));
            }

            let mut additional = 0u64;
            let mut to_write = Vec::new();
            for (hash, size, bytes) in &to_insert {
                let path = blob_file(&blobs, hash)?;
                if !path.exists() {
                    additional += *size;
                    to_write.push((path, bytes.clone()));
                }
            }
            let used = dir_size(&blobs);
            if used.saturating_add(additional) > max_group_bytes {
                return Err(StoreError::QuotaExceeded { used, additional });
            }

            std::fs::create_dir_all(&blobs).context("创建图片目录失败")?;
            let mut written = Vec::new();
            for (path, bytes) in &to_write {
                write_blob_atomic(path, bytes)?;
                written.push(path.clone());
            }

            if let Err(error) = insert_images(&pool, &library, &to_insert).await {
                for path in written {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error);
            }

            Ok(AddResult {
                added: to_insert.len(),
                skipped_dup,
            })
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
                let _ = std::fs::remove_file(path);
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
            let hashes = library_hashes(&pool, &canonical).await?;
            sqlx::query("DELETE FROM images WHERE library = ?")
                .bind(&canonical)
                .execute(&pool)
                .await?;
            sqlx::query("DELETE FROM aliases WHERE target = ? OR alias = ?")
                .bind(&canonical)
                .bind(&canonical)
                .execute(&pool)
                .await?;
            for hash in hashes {
                if !hash_still_used(&pool, &hash).await?
                    && let Ok(path) = blob_file(&blobs, &hash)
                {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(canonical)
        })
        .await
    }

    pub async fn pick_random(&self, group_id: i64, name: &str) -> Result<String, StoreError> {
        self.with_group(group_id, |pool| async move {
            let library = resolve_library(&pool, name).await?;
            let hash = sqlx::query_scalar::<_, String>(
                "SELECT hash FROM images WHERE library = ? ORDER BY RANDOM() LIMIT 1",
            )
            .bind(&library)
            .fetch_optional(&pool)
            .await?;
            hash.ok_or(StoreError::LibraryEmpty)
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
        let blobs = self.blobs_dir(group_id);
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
            let unique_bytes = sqlx::query_scalar::<_, Option<i64>>(
                "SELECT SUM(size) FROM (SELECT hash, MAX(size) AS size FROM images GROUP BY hash)",
            )
            .fetch_one(&pool)
            .await?
            .unwrap_or(0) as u64;
            Ok(GroupStats {
                libraries,
                unique_count,
                unique_bytes: unique_bytes.max(dir_size(&blobs)),
            })
        })
        .await
    }

    pub fn read_blob(&self, group_id: i64, hash: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(group_id, hash)?;
        std::fs::read(&path).with_context(|| format!("读取图片失败: {}", path.display()))
    }
}

async fn init_schema(pool: &SqlitePool) -> Result<(), StoreError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS images (
            library TEXT NOT NULL,
            hash TEXT NOT NULL CHECK (length(hash) = 64),
            size INTEGER NOT NULL CHECK (size > 0),
            PRIMARY KEY (library, hash)
        )",
    )
    .execute(pool)
    .await?;
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
    Ok(())
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

async fn hash_still_used(pool: &SqlitePool, hash: &str) -> Result<bool, StoreError> {
    let found = sqlx::query_scalar::<_, i64>("SELECT 1 FROM images WHERE hash = ? LIMIT 1")
        .bind(hash)
        .fetch_optional(pool)
        .await?;
    Ok(found.is_some())
}

async fn insert_images(
    pool: &SqlitePool,
    library: &str,
    images: &[(String, u64, Vec<u8>)],
) -> Result<(), StoreError> {
    for (hash, size, _) in images {
        sqlx::query("INSERT OR IGNORE INTO images (library, hash, size) VALUES (?, ?, ?)")
            .bind(library)
            .bind(hash)
            .bind(*size as i64)
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

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_none())
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| meta.len())
        .sum()
}

fn is_blob_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn write_blob_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    restrict_file_permissions(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (Store, PathBuf) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("image_lib_store_{}_{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(dir.clone()).unwrap(), dir)
    }

    fn png_like(tag: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG".to_vec();
        bytes.extend_from_slice(&[tag; 32]);
        bytes
    }

    #[tokio::test]
    async fn adds_dedups_shares_blob_and_deletes() {
        let (store, dir) = temp_store();
        let group = 1;
        let a = png_like(1);
        let b = png_like(2);

        let first = store
            .add_images(group, "猫", vec![a.clone()])
            .await
            .unwrap();
        assert_eq!(
            first,
            AddResult {
                added: 1,
                skipped_dup: 0
            }
        );
        let again = store
            .add_images(group, "猫", vec![a.clone()])
            .await
            .unwrap();
        assert_eq!(
            again,
            AddResult {
                added: 0,
                skipped_dup: 1
            }
        );
        store
            .add_images(group, "狗", vec![a.clone(), b.clone()])
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
    async fn alias_resolves_and_wipe_clears_canonical() {
        let (store, dir) = temp_store();
        let group = 2;
        store
            .add_images(group, "猫", vec![png_like(1)])
            .await
            .unwrap();
        let canonical = store.set_alias(group, "喵", "猫").await.unwrap();
        assert_eq!(canonical, "猫");
        assert!(store.pick_random(group, "喵").await.is_ok());

        store
            .add_images(group, "喵", vec![png_like(2)])
            .await
            .unwrap();
        let stats = store.stats(group).await.unwrap();
        assert_eq!(stats.libraries.len(), 1);
        assert_eq!(stats.libraries[0].name, "猫");
        assert_eq!(stats.libraries[0].aliases, vec!["喵".to_owned()]);
        assert_eq!(stats.libraries[0].count, 2);

        let wiped = store.wipe_library(group, "喵").await.unwrap();
        assert_eq!(wiped, "猫");
        assert!(store.stats(group).await.unwrap().libraries.is_empty());
        assert!(matches!(
            store.pick_random(group, "喵").await,
            Err(StoreError::LibraryEmpty)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn alias_rejects_existing_library_and_missing_target() {
        let (store, dir) = temp_store();
        let group = 3;
        store
            .add_images(group, "猫", vec![png_like(1)])
            .await
            .unwrap();
        store
            .add_images(group, "狗", vec![png_like(2)])
            .await
            .unwrap();
        assert!(matches!(
            store.set_alias(group, "狗", "猫").await,
            Err(StoreError::NameIsLibrary(_))
        ));
        assert!(matches!(
            store.set_alias(group, "喵", "龙").await,
            Err(StoreError::TargetMissing(_))
        ));
        assert!(matches!(
            store.set_alias(group, "猫", "猫").await,
            Err(StoreError::AliasToSelf)
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rejects_when_group_quota_would_exceed() {
        let dir = std::env::temp_dir().join(format!("image_lib_quota_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open_with_quota(dir.clone(), 40).unwrap();
        store.add_images(7, "小", vec![png_like(3)]).await.unwrap();
        let err = store
            .add_images(7, "小", vec![png_like(4)])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::QuotaExceeded { .. }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blob_path_rejects_non_hex_hash() {
        let (store, dir) = temp_store();
        assert!(store.read_blob(1, "../passwd").is_err());
        assert!(store.read_blob(1, "zz").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
