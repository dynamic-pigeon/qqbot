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
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Subscribe {
    pub uid: u64,
    pub groups: Vec<i64>,
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
        }
    };
    CONFIG
        .set((ArcSwap::new(Arc::new(config)), config_path))
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
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
