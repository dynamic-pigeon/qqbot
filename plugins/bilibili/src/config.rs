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
    /// 群身份键：官方栈为 group_openid，OneBot 栈为群号十进制串。
    /// 旧配置里的数字群号统一读成十进制串。
    #[serde(default, deserialize_with = "de_group_list")]
    pub groups: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DynamicCheckpoint {
    pub uid: u64,
    #[serde(deserialize_with = "de_group_id")]
    pub group: String,
    pub last_seen: i64,
}

fn de_group_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum GroupId {
        Number(i64),
        Text(String),
    }

    let raw = Vec::<GroupId>::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|id| match id {
            GroupId::Number(number) => number.to_string(),
            GroupId::Text(text) => text,
        })
        .collect())
}

fn de_group_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum GroupId {
        Number(i64),
        Text(String),
    }

    match GroupId::deserialize(deserializer)? {
        GroupId::Number(number) => Ok(number.to_string()),
        GroupId::Text(text) => Ok(text),
    }
}

/// 把 group 加入 uid 的订阅组列表；uid 尚无条目时新建一条。
/// 返回是否实际发生变更（group 已在列表中时返回 false）。
pub(crate) fn upsert_subscribe_group(
    subscribe: &mut Vec<Subscribe>,
    uid: u64,
    group: &str,
) -> bool {
    if let Some(subscription) = subscribe.iter_mut().find(|item| item.uid == uid) {
        if subscription.groups.iter().any(|item| item == group) {
            return false;
        }
        subscription.groups.push(group.to_owned());
    } else {
        subscribe.push(Subscribe {
            uid,
            groups: vec![group.to_owned()],
        });
    }
    true
}

/// 从 uid 的订阅中移除 group；组列表清空后整条订阅一并删除。
pub(crate) fn remove_subscribe_group(subscribe: &mut Vec<Subscribe>, uid: u64, group: &str) {
    if let Some(index) = subscribe.iter().position(|item| item.uid == uid) {
        let subscription = &mut subscribe[index];
        subscription.groups.retain(|item| item != group);
        if subscription.groups.is_empty() {
            subscribe.remove(index);
        }
    }
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
