use std::path::PathBuf;

use anyhow::Result;
use kovi::tokio::sync::RwLockReadGuard;

static CONFIG: kovi::tokio::sync::OnceCell<kovi::tokio::sync::RwLock<Config>> =
    kovi::tokio::sync::OnceCell::const_new();

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct Config {
    pub wordcloud_cli_path: String,
    pub notify_group: Vec<i64>,
    pub tencent: Option<TencentCloudConfig>,
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
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

pub async fn init_config(path: PathBuf) -> Result<()> {
    let mut config: Config = kovi::utils::load_json_data(Default::default(), &path).unwrap();
    config.path = path;
    let rw_lock = kovi::tokio::sync::RwLock::new(config);
    CONFIG
        .set(rw_lock)
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

#[inline(always)]
pub async fn modify_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    let cfg = CONFIG.get().unwrap();
    let mut config = cfg.write().await;
    f(&mut config);
    write_config(&mut config).await
}

#[inline(always)]
pub async fn read_config<'a>() -> RwLockReadGuard<'a, Config> {
    let cfg = CONFIG.get().unwrap();
    cfg.read().await
}

pub async fn write_config(config: &mut Config) -> Result<()> {
    let config_path = &config.path;
    match kovi::utils::save_json_data(&*config, config_path) {
        Err(e) => {
            anyhow::bail!("保存配置文件失败: {}", e);
        }
        Ok(_) => Ok(()),
    }
}
