use std::sync::Arc;

use kovi::PluginBuilder as plugin;
use utils::JsonStore;

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

static CONFIG: std::sync::OnceLock<JsonStore<Config>> = std::sync::OnceLock::new();

pub fn init() -> anyhow::Result<()> {
    let bot = plugin::get_runtime_bot();
    let store = JsonStore::open(bot.get_data_path().join("config.json"))?;
    CONFIG
        .set(store)
        .map_err(|_| anyhow::anyhow!("配置已初始化"))?;
    Ok(())
}

pub fn read_config() -> Arc<Config> {
    CONFIG.get().expect("配置未初始化").get()
}

pub fn modify_config<F>(f: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut Config),
{
    CONFIG.get().expect("配置未初始化").modify(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kovi::serde_json;

    #[test]
    fn config_missing_fields_default_to_empty() {
        let cfg: Config = serde_json::from_str(r#"{"subscribe":[]}"#).unwrap();
        assert!(cfg.dynamic_subscribe.is_empty());
        assert!(cfg.dynamic_checkpoints.is_empty());
    }
}
