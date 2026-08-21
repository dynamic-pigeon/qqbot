use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use kovi::{
    PluginBuilder as plugin, serde_json,
    tokio::{self, sync::OnceCell},
};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct Config {
    pub subscribe: Vec<Subscribe>,
    #[serde(default)]
    pub dynamic_subscribe: Vec<DynamicSubscribe>,
    #[serde(default)]
    pub dynamic_checkpoints: Vec<DynamicCheckpoint>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Subscribe {
    pub uid: u64,
    pub groups: Vec<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct DynamicSubscribe {
    pub uid: u64,
    pub groups: Vec<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DynamicCheckpoint {
    pub uid: u64,
    pub group: i64,
    pub last_seen: i64,
}

static CONFIG: OnceCell<(ArcSwap<Config>, PathBuf)> = OnceCell::const_new();

pub async fn init() -> anyhow::Result<()> {
    let bot = plugin::get_runtime_bot();
    let path = bot.get_data_path();
    let config_path = path.join("config.json");
    let config = if config_path.exists() {
        let data = tokio::fs::read(&config_path).await?;
        match serde_json::from_slice(&data) {
            Ok(config) => config,
            Err(error) => {
                // 配置损坏（如上次写入中途崩溃）不应击垮插件启动：
                // 备份原文件后回退空配置，订阅可重新添加。
                let backup = config_path.with_extension("json.bak");
                tracing::error!(
                    "bilibili 配置解析失败: {error}，备份到 {} 并回退空配置",
                    backup.display()
                );
                if let Err(error) = tokio::fs::rename(&config_path, &backup).await {
                    tracing::warn!("备份损坏的配置文件失败: {error}");
                }
                Config::default()
            }
        }
    } else {
        Config::default()
    };
    if !config_path.exists() {
        write_config(&config, &config_path)?;
    }
    restrict_config_permissions(&config_path)?;
    CONFIG
        .set((ArcSwap::new(Arc::new(config)), config_path))
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_config_permissions(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_config_permissions(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[inline(always)]
pub fn read_config() -> Arc<Config> {
    let config = CONFIG.get().expect("配置未初始化");
    config.0.load_full()
}

#[cold]
#[inline(never)]
pub async fn modify_config<F>(mut f: F) -> anyhow::Result<()>
where
    F: FnMut(&mut Config),
{
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = LOCK.lock().await;
    let config = CONFIG.get().expect("配置未初始化");
    let mut new_config = config.0.load_full().as_ref().clone();
    f(&mut new_config);
    write_config(&new_config, &config.1)?;
    let new_config = Arc::new(new_config);
    config.0.store(new_config);
    Ok(())
}

pub fn write_config(config: &Config, path: impl AsRef<Path>) -> anyhow::Result<()> {
    let path = path.as_ref();
    // 先写临时文件再 rename：checkpoint 每轮 poll 都会写盘，直接截断写一旦
    // 中途崩溃会留下半个 JSON。同目录 rename 是原子的。
    let tmp_path = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(config)?;
    std::fs::write(&tmp_path, data)?;
    // rename 会用临时文件替换目标，权限需在替换前收紧，否则回退为 umask 默认值。
    restrict_config_permissions(&tmp_path)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_config_roundtrip_and_cleans_up_tmp() {
        let dir = std::env::temp_dir().join(format!("bili_config_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let cfg = Config {
            dynamic_checkpoints: vec![DynamicCheckpoint {
                uid: 1,
                group: 2,
                last_seen: 3,
            }],
            ..Config::default()
        };
        write_config(&cfg, &path).unwrap();
        let back: Config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.dynamic_checkpoints, cfg.dynamic_checkpoints);
        assert!(!dir.join("config.json.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_missing_fields_default_to_empty() {
        // 缺字段时 serde 走 Default，动态订阅和 checkpoint 都为空。
        let cfg: Config = serde_json::from_str(r#"{"subscribe":[]}"#).unwrap();
        assert!(cfg.dynamic_subscribe.is_empty());
        assert!(cfg.dynamic_checkpoints.is_empty());
    }
}
