use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};

use anyhow::Result;
use utils::JsonStore;

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

static CONFIG: OnceLock<JsonStore<Config>> = OnceLock::new();

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct Config {
    pub notify_group: Vec<i64>,
    /// 词云背景色，支持 #RRGGBB 和常见颜色名。
    #[serde(default = "default_wordcloud_background")]
    pub wordcloud_background: String,
}

fn default_wordcloud_background() -> String {
    "#ffffff".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            notify_group: vec![],
            wordcloud_background: default_wordcloud_background(),
        }
    }
}

pub fn init_config(path: PathBuf) -> Result<()> {
    let store = JsonStore::open(path)?;
    CONFIG
        .set(store)
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

pub fn read_config() -> Arc<Config> {
    CONFIG.get().expect("配置未初始化").get()
}

pub fn modify_config<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut Config),
{
    CONFIG.get().expect("配置未初始化").modify(f)
}
