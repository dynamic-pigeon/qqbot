use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rand::seq::IndexedRandom as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GroupIndex {
    libraries: BTreeMap<String, Library>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Library {
    images: Vec<ImageMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImageMeta {
    hash: String,
    size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddResult {
    pub added: usize,
    pub skipped_dup: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryStat {
    pub name: String,
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

    fn index_path(&self, group_id: i64) -> PathBuf {
        self.group_dir(group_id).join("index.json")
    }

    fn blobs_dir(&self, group_id: i64) -> PathBuf {
        self.group_dir(group_id).join("blobs")
    }

    fn blob_path(&self, group_id: i64, hash: &str) -> Result<PathBuf> {
        if !is_blob_hash(hash) {
            anyhow::bail!("非法图片哈希");
        }
        Ok(self.blobs_dir(group_id).join(hash))
    }

    async fn with_group<T>(
        &self,
        group_id: i64,
        f: impl FnOnce(&Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let lock = self.group_lock(group_id);
        let _guard = lock.lock().await;
        f(self)
    }

    fn blob_dir_size(&self, group_id: i64) -> u64 {
        let Ok(entries) = std::fs::read_dir(self.blobs_dir(group_id)) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_none())
            .filter_map(|entry| entry.metadata().ok())
            .map(|meta| meta.len())
            .sum()
    }

    fn load_index(&self, group_id: i64) -> Result<GroupIndex> {
        let path = self.index_path(group_id);
        if !path.exists() {
            return Ok(GroupIndex::default());
        }
        let data = std::fs::read(&path)
            .with_context(|| format!("读取图库索引失败: {}", path.display()))?;
        serde_json::from_slice(&data).context("解析图库索引失败")
    }

    fn save_index(&self, group_id: i64, index: &GroupIndex) -> Result<()> {
        let dir = self.group_dir(group_id);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(self.blobs_dir(group_id))?;
        let path = self.index_path(group_id);
        // 先写临时文件再 rename，避免写到一半崩溃留下半份 JSON。
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(index)?;
        std::fs::write(&tmp, data)?;
        restrict_file_permissions(&tmp)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
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

        self.with_group(group_id, |store| {
            let mut index = store.load_index(group_id)?;
            let existing: HashSet<String> = index
                .libraries
                .get(name)
                .map(|library| {
                    library
                        .images
                        .iter()
                        .map(|item| item.hash.clone())
                        .collect()
                })
                .unwrap_or_default();

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
                let path = store.blob_path(group_id, hash)?;
                if !path.exists() {
                    additional += *size;
                    to_write.push((path, bytes.as_slice()));
                }
            }
            // 按目录实占计配额，把写失败留下的孤儿 blob 也算进去。
            let used = store.blob_dir_size(group_id);
            if used.saturating_add(additional) > store.max_group_bytes {
                return Err(StoreError::QuotaExceeded { used, additional });
            }

            std::fs::create_dir_all(store.blobs_dir(group_id)).context("创建图片目录失败")?;
            let mut written = Vec::new();
            for (path, bytes) in to_write {
                write_blob_atomic(&path, bytes)?;
                written.push(path);
            }

            let library = index.libraries.entry(name.to_owned()).or_default();
            for (hash, size, _) in &to_insert {
                library.images.push(ImageMeta {
                    hash: hash.clone(),
                    size: *size,
                });
            }

            if let Err(error) = store.save_index(group_id, &index) {
                for path in written {
                    let _ = std::fs::remove_file(path);
                }
                return Err(error.into());
            }

            Ok(AddResult {
                added: to_insert.len(),
                skipped_dup,
            })
        })
        .await
    }

    pub async fn delete_hash(&self, group_id: i64, hash: &str) -> Result<Vec<String>, StoreError> {
        self.with_group(group_id, |store| {
            let mut index = store.load_index(group_id)?;
            let mut hit = Vec::new();
            for (name, library) in index.libraries.iter_mut() {
                let before = library.images.len();
                library.images.retain(|item| item.hash != hash);
                if library.images.len() != before {
                    hit.push(name.clone());
                }
            }
            if hit.is_empty() {
                return Err(StoreError::ImageMissing);
            }
            index
                .libraries
                .retain(|_, library| !library.images.is_empty());
            store.save_index(group_id, &index)?;
            if !referenced_hashes(&index).contains(hash)
                && let Ok(path) = store.blob_path(group_id, hash)
            {
                let _ = std::fs::remove_file(path);
            }
            Ok(hit)
        })
        .await
    }

    pub async fn wipe_library(&self, group_id: i64, name: &str) -> Result<(), StoreError> {
        self.with_group(group_id, |store| {
            let mut index = store.load_index(group_id)?;
            let Some(library) = index.libraries.remove(name) else {
                return Err(StoreError::LibraryMissing);
            };
            store.save_index(group_id, &index)?;
            let still = referenced_hashes(&index);
            for image in library.images {
                if !still.contains(image.hash.as_str())
                    && let Ok(path) = store.blob_path(group_id, &image.hash)
                {
                    let _ = std::fs::remove_file(path);
                }
            }
            Ok(())
        })
        .await
    }

    pub async fn pick_random(&self, group_id: i64, name: &str) -> Result<String, StoreError> {
        self.with_group(group_id, |store| {
            let index = store.load_index(group_id)?;
            let library = index
                .libraries
                .get(name)
                .ok_or(StoreError::LibraryMissing)?;
            if library.images.is_empty() {
                return Err(StoreError::LibraryEmpty);
            }
            let chosen = library
                .images
                .choose(&mut rand::rng())
                .ok_or(StoreError::LibraryEmpty)?;
            Ok(chosen.hash.clone())
        })
        .await
    }

    pub async fn stats(&self, group_id: i64) -> Result<GroupStats, StoreError> {
        self.with_group(group_id, |store| {
            let index = store.load_index(group_id)?;
            let libraries = index
                .libraries
                .iter()
                .map(|(name, library)| LibraryStat {
                    name: name.clone(),
                    count: library.images.len(),
                    bytes: library.images.iter().map(|item| item.size).sum(),
                })
                .collect();
            Ok(GroupStats {
                libraries,
                unique_count: referenced_hashes(&index).len(),
                unique_bytes: unique_bytes(&index).max(store.blob_dir_size(group_id)),
            })
        })
        .await
    }

    pub fn read_blob(&self, group_id: i64, hash: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(group_id, hash)?;
        std::fs::read(&path).with_context(|| format!("读取图片失败: {}", path.display()))
    }
}

fn is_blob_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn referenced_hashes(index: &GroupIndex) -> HashSet<&str> {
    index
        .libraries
        .values()
        .flat_map(|library| library.images.iter().map(|item| item.hash.as_str()))
        .collect()
}

fn unique_bytes(index: &GroupIndex) -> u64 {
    let mut seen = HashSet::new();
    let mut total = 0;
    for library in index.libraries.values() {
        for image in &library.images {
            if seen.insert(image.hash.as_str()) {
                total += image.size;
            }
        }
    }
    total
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
