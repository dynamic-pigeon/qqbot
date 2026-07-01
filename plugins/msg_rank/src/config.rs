use std::path::PathBuf;

use anyhow::Result;
use utils::{RcuCell, RcuReadGuard};

pub(crate) static CONFIG: kovi::tokio::sync::OnceCell<RcuCell<Config>> =
    kovi::tokio::sync::OnceCell::const_new();
static CONFIG_WRITE_LOCK: kovi::tokio::sync::Mutex<()> = kovi::tokio::sync::Mutex::const_new(());

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Config {
    /// 已废弃：新实现使用 Playwright 截图，不再调用外部 wordcloud_cli。
    /// 保留字段是为了兼容旧配置文件，避免反序列化失败。
    #[serde(default)]
    pub wordcloud_cli_path: String,
    pub notify_group: Vec<i64>,
    pub tencent: Option<TencentCloudConfig>,
    #[serde(default = "default_wordcloud_background")]
    pub wordcloud_background: String,
    #[serde(skip)]
    pub path: PathBuf,
}

fn default_wordcloud_background() -> String {
    "white".to_string()
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
            wordcloud_background: default_wordcloud_background(),
            path: PathBuf::new(),
        }
    }
}

pub(crate) type ConfigGuard<'a> = RcuReadGuard<'a, Config>;

pub async fn init_config(path: PathBuf) -> Result<()> {
    let mut config: Config = kovi::utils::load_json_data(Default::default(), &path)
        .map_err(|e| anyhow::anyhow!("加载配置文件失败: {e}"))?;
    config.path = path;
    let rcu = RcuCell::new(config);

    CONFIG
        .set(rcu)
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

#[inline(always)]
pub fn read_config() -> ConfigGuard<'static> {
    let config = CONFIG.get().expect("配置未初始化");
    config.read()
}

#[cold]
#[inline(never)]
pub async fn modify_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    // 写路径串行化，避免并发写导致配置覆盖；读路径仍保持无锁快照读取。
    let _write_guard = CONFIG_WRITE_LOCK.lock().await;

    let cfg = CONFIG.get().unwrap();
    let mut next = cfg.snapshot();
    f(&mut next);
    // 调用频率不高，直接每次修改都写入文件，保证配置的持久化
    write_config(&next)?;
    cfg.replace(next);
    Ok(())
}

pub fn write_config(config: &Config) -> Result<()> {
    let config_path = &config.path;
    match kovi::utils::save_json_data(config, config_path) {
        Err(e) => {
            anyhow::bail!("保存配置文件失败: {}", e);
        }
        Ok(_) => Ok(()),
    }
}
