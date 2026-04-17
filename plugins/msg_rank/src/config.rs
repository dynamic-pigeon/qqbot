use std::path::PathBuf;

use anyhow::Result;
use crossbeam_epoch::{Atomic, Owned};

pub(crate) static CONFIG: kovi::tokio::sync::OnceCell<Atomic<Config>> =
    kovi::tokio::sync::OnceCell::const_new();
static CONFIG_WRITE_LOCK: kovi::tokio::sync::Mutex<()> = kovi::tokio::sync::Mutex::const_new(());

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Config {
    pub wordcloud_cli_path: String,
    pub notify_group: Vec<i64>,
    pub tencent: Option<TencentCloudConfig>,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct TencentCloudConfig {
    #[serde(rename = "SecretId")]
    pub secret_id: String,
    #[serde(rename = "SecretKey")]
    pub secret_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            wordcloud_cli_path: "wordcloud_cli".to_string(),
            notify_group: vec![],
            tencent: None,
            path: PathBuf::new(),
        }
    }
}

pub struct ConfigGuard<'a> {
    guard: crossbeam_epoch::Guard,
    config: &'a Atomic<Config>,
}

impl<'a> ConfigGuard<'a> {
    fn new(config: &'a Atomic<Config>) -> Self {
        let guard = crossbeam_epoch::pin();
        Self { guard, config }
    }

    fn load(&self) -> &Config {
        let ptr = self
            .config
            .load(std::sync::atomic::Ordering::Relaxed, &self.guard);
        unsafe { ptr.deref() }
    }
}

impl<'a> std::ops::Deref for ConfigGuard<'a> {
    type Target = Config;

    fn deref(&self) -> &Self::Target {
        self.load()
    }
}

pub fn read_config<'a>() -> ConfigGuard<'a> {
    let config = CONFIG.get().expect("配置未初始化");
    ConfigGuard::new(config)
}

pub async fn init_config(path: PathBuf) -> Result<()> {
    let mut config: Config = kovi::utils::load_json_data(Default::default(), &path).unwrap();
    config.path = path;
    let rcu = Atomic::new(config);

    CONFIG
        .set(rcu)
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

#[inline(always)]
pub async fn modify_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    // 写路径串行化，避免并发写导致配置覆盖；读路径仍保持无锁快照读取。
    let _write_guard = CONFIG_WRITE_LOCK.lock().await;

    let cfg = CONFIG.get().unwrap();
    let guard = &crossbeam_epoch::pin();
    guard.flush();
    let current = cfg.load(std::sync::atomic::Ordering::Relaxed, guard);
    let mut next = unsafe { current.deref().clone() };
    f(&mut next);
    // 调用频率不高，直接每次修改都写入文件，保证配置的持久化
    write_config(&next)?;
    let p = cfg.swap(
        Owned::new(next),
        std::sync::atomic::Ordering::Relaxed,
        guard,
    );
    unsafe {
        guard.defer_destroy(p);
    }
    Ok(())
}

pub fn write_config(config: &Config) -> Result<()> {
    let config_path = &config.path;
    match kovi::utils::save_json_data(&*config, config_path) {
        Err(e) => {
            anyhow::bail!("保存配置文件失败: {}", e);
        }
        Ok(_) => Ok(()),
    }
}
