use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use kovi::{
    PluginBuilder as plugin, serde_json,
    tokio::{self, sync::OnceCell},
};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
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
        serde_json::from_slice(&data)?
    } else {
        Config {
            subscribe: Vec::new(),
            dynamic_subscribe: Vec::new(),
            dynamic_checkpoints: Vec::new(),
        }
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
    match kovi::utils::save_json_data(config, path) {
        Err(e) => {
            anyhow::bail!("保存配置文件失败: {}", e);
        }
        Ok(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_backward_compatible_without_dynamic_subscribe() {
        // 旧 config.json 缺字段时，serde 默认值兜底
        let old = r#"{"subscribe":[]}"#;
        let cfg: Config = serde_json::from_str(old).unwrap();
        assert!(cfg.dynamic_subscribe.is_empty());
        assert!(cfg.dynamic_checkpoints.is_empty());
    }

    #[test]
    fn config_roundtrip_with_dynamic_subscribe() {
        let cfg = Config {
            subscribe: vec![],
            dynamic_subscribe: vec![DynamicSubscribe {
                uid: 1,
                groups: vec![100],
            }],
            dynamic_checkpoints: vec![DynamicCheckpoint {
                uid: 1,
                group: 100,
                last_seen: 123,
            }],
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(back.dynamic_subscribe.len(), 1);
        assert_eq!(back.dynamic_subscribe[0].uid, 1);
        assert_eq!(back.dynamic_subscribe[0].groups, vec![100]);
        assert_eq!(back.dynamic_checkpoints, cfg.dynamic_checkpoints);
    }
}
