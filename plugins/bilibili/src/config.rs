use kovi::PluginBuilder as plugin;
use utils::JsonStore;

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct Config {
    pub subscribe: Vec<Subscribe>,
    #[serde(default)]
    pub dynamic_subscribe: Vec<Subscribe>,
    #[serde(default)]
    pub dynamic_checkpoints: Vec<DynamicCheckpoint>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct Subscribe {
    pub uid: u64,
    pub groups: Vec<i64>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DynamicCheckpoint {
    pub uid: u64,
    pub group: i64,
    pub last_seen: i64,
}

pub(crate) static CONFIG: JsonStore<Config> = JsonStore::new();

pub fn init() -> anyhow::Result<()> {
    let bot = plugin::get_runtime_bot();
    CONFIG.init(bot.get_data_path().join("config.json"))
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
