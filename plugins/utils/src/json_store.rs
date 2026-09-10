use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwap;
use serde::{Serialize, de::DeserializeOwned};

use crate::restrict_mode_0600;

/// 运行时 JSON 配置：内存快照无锁读，写路径串行并原子落盘。
pub struct JsonStore<T> {
    path: PathBuf,
    data: ArcSwap<T>,
    write_lock: Mutex<()>,
}

impl<T> JsonStore<T>
where
    T: Serialize + DeserializeOwned + Default + Clone,
{
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let value = load_or_default(&path)?;
        persist_if_missing(&path, &value)?;
        restrict_mode_0600(&path)?;
        Ok(Self {
            path,
            data: ArcSwap::from_pointee(value),
            write_lock: Mutex::new(()),
        })
    }

    pub fn get(&self) -> Arc<T> {
        self.data.load_full()
    }

    pub fn modify<F>(&self, f: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut T),
    {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut next = self.data.load_full().as_ref().clone();
        f(&mut next);
        write_atomic(&self.path, &next)?;
        self.data.store(Arc::new(next));
        Ok(())
    }
}

fn load_or_default<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    if !path.is_file() {
        return Ok(T::default());
    }
    let bytes = fs::read(path)?;
    match kovi::serde_json::from_slice(&bytes) {
        Ok(value) => Ok(value),
        Err(error) => {
            let backup = path.with_extension("json.bak");
            tracing::error!(
                path = %path.display(),
                backup = %backup.display(),
                "配置解析失败，备份后回退默认配置: {error}"
            );
            if let Err(error) = fs::rename(path, &backup) {
                tracing::warn!(path = %path.display(), "备份损坏的配置文件失败: {error}");
            }
            Ok(T::default())
        }
    }
}

fn persist_if_missing<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if path.is_file() {
        return Ok(());
    }
    write_atomic(path, value)
}

fn write_atomic<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let data = kovi::serde_json::to_vec_pretty(value)?;
    let write = (|| {
        fs::write(&tmp_path, &data)?;
        restrict_mode_0600(&tmp_path)?;
        fs::rename(&tmp_path, path)?;
        anyhow::Ok(())
    })();
    if write.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct Sample {
        value: u32,
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "json_store_{name}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir.join("config.json")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn missing_file_writes_default() {
        let path = temp_path("missing");
        let store = JsonStore::<Sample>::open(&path).unwrap();
        assert_eq!(store.get().value, 0);
        assert!(path.is_file());
        assert!(!path.with_extension("json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        cleanup(&path);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_reset() {
        let path = temp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{not json").unwrap();
        let store = JsonStore::<Sample>::open(&path).unwrap();
        assert_eq!(store.get().value, 0);
        let backup = path.with_extension("json.bak");
        assert!(backup.is_file());
        assert_eq!(fs::read_to_string(&backup).unwrap(), "{not json");
        let restored: Sample = kovi::serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(restored, Sample::default());
        cleanup(&path);
    }

    #[test]
    fn modify_atomically_persists() {
        let path = temp_path("modify");
        let store = JsonStore::<Sample>::open(&path).unwrap();
        store.modify(|sample| sample.value = 7).unwrap();
        assert_eq!(store.get().value, 7);
        assert!(!path.with_extension("json.tmp").exists());
        let reopened = JsonStore::<Sample>::open(&path).unwrap();
        assert_eq!(reopened.get().value, 7);
        cleanup(&path);
    }
}
