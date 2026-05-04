use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

use kovi::{
    serde_json,
    tokio::{self, sync::OnceCell},
};
use utils::RcuCell;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Config {
    pub subscribe: Vec<Subscribe>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Subscribe {
    pub uid: u64,
    pub groups: Vec<i64>,
}

static CONFIG: OnceCell<(RcuCell<Config>, PathBuf)> = OnceCell::const_new();

pub async fn init_config(path: PathBuf) -> anyhow::Result<()> {
    let config = if path.exists() {
        let data = tokio::fs::read(&path).await?;
        serde_json::from_slice(&data)?
    } else {
        Config {
            subscribe: Vec::new(),
        }
    };
    CONFIG
        .set((RcuCell::new(config), path))
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

type ConfigGuard<'a> = utils::RcuReadGuard<'a, Config>;

#[inline(always)]
pub fn read_config() -> ConfigGuard<'static> {
    let config = CONFIG.get().expect("配置未初始化");
    config.0.read()
}

#[cold]
#[inline(never)]
pub async fn modify_config<F>(f: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Config),
{
    static CONFIG_WRITE_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));
    // 写路径串行化，避免并发写导致配置覆盖；读路径仍保持无锁快照读取。
    let _write_guard = CONFIG_WRITE_LOCK.lock().await;

    let config = CONFIG.get().unwrap();
    let cfg = &config.0;
    let mut next = cfg.snapshot();
    f(&mut next);
    // 调用频率不高，直接每次修改都写入文件，保证配置的持久化
    write_config(&next, &config.1)?;
    cfg.replace(next);
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
