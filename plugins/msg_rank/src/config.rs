use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::Result;
use utils::{RcuCell, RcuReadGuard};

/// 根目录 `config.toml` 的 `[msg_rank]`。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default)]
pub(crate) struct StaticConfig {
    pub retention_days: u64,
    pub wordcloud_concurrency: usize,
    pub wordcloud: Vec<WordCloudSchedule>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct WordCloudSchedule {
    pub cron: String,
    pub days: i64,
    pub title: String,
}

impl Default for StaticConfig {
    fn default() -> Self {
        Self {
            retention_days: 8,
            wordcloud_concurrency: 1,
            wordcloud: vec![
                WordCloudSchedule {
                    cron: "0 21 * * *".into(),
                    days: 1,
                    title: "今日词云".into(),
                },
                WordCloudSchedule {
                    cron: "0 10 * * 6".into(),
                    days: 7,
                    title: "上周词云".into(),
                },
            ],
        }
    }
}

pub(crate) fn static_config() -> &'static StaticConfig {
    static CONFIG: LazyLock<StaticConfig> = LazyLock::new(|| {
        utils::config::parse("msg_rank")
            .unwrap_or_else(|error| panic!("解析 [msg_rank] 配置失败: {error:#}"))
    });
    &CONFIG
}

pub(crate) static CONFIG: kovi::tokio::sync::OnceCell<RcuCell<Config>> =
    kovi::tokio::sync::OnceCell::const_new();
static CONFIG_WRITE_LOCK: kovi::tokio::sync::Mutex<()> = kovi::tokio::sync::Mutex::const_new(());

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Config {
    pub notify_group: Vec<i64>,
    /// 词云背景色，支持 #RRGGBB 和常见颜色名。
    #[serde(default = "default_wordcloud_background")]
    pub wordcloud_background: String,
    #[serde(skip)]
    pub path: PathBuf,
}

fn default_wordcloud_background() -> String {
    "#ffffff".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notify_group: vec![],
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
    if !config.path.exists() {
        write_config(&config)?;
    }
    restrict_config_permissions(&config.path)?;
    let rcu = RcuCell::new(config);

    CONFIG
        .set(rcu)
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

#[cfg(unix)]
fn restrict_config_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_config_permissions(_path: &std::path::Path) -> Result<()> {
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
