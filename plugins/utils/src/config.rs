//! 当前目录 `config.toml`。读成 TOML [`Value`]，各插件用 [`parse`] 反序列化自己的表。

use std::sync::LazyLock;

use kovi::toml;
use serde::de::DeserializeOwned;

pub use kovi::toml::Value;

/// 根目录 `config.toml` 的 `[network]`。
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NetworkConfig {
    pub private_network_protection: bool,
}

/// 根目录 `config.toml`。找不到文件时为空表。
pub fn value() -> &'static Value {
    static CONFIG: LazyLock<Value> =
        LazyLock::new(|| load().unwrap_or_else(|error| panic!("加载全局配置失败: {error:#}")));
    &CONFIG
}

pub(crate) fn network() -> &'static NetworkConfig {
    static CONFIG: LazyLock<NetworkConfig> = LazyLock::new(|| parse_or_panic("network"));
    &CONFIG
}

/// 读盘并校验 utils 自己消费的 `[network]`。其它段由各插件在启动时 parse。
pub fn preload() {
    let _ = network();
}

/// 把顶层键反序列化成调用方类型。键不存在时按空表解析。
pub fn parse<T: DeserializeOwned>(key: &str) -> anyhow::Result<T> {
    value()
        .get(key)
        .cloned()
        .unwrap_or_else(|| Value::Table(toml::Table::new()))
        .try_into()
        .map_err(|error| anyhow::anyhow!("解析 config.toml 的 [{key}] 失败: {error}"))
}

pub fn parse_or_panic<T: DeserializeOwned>(key: &str) -> T {
    parse(key).unwrap_or_else(|error| panic!("解析 config.toml 的 [{key}] 失败: {error:#}"))
}

fn load() -> anyhow::Result<Value> {
    let path = std::path::Path::new("config.toml");
    if !path.is_file() {
        tracing::debug!("未找到 config.toml，使用空配置");
        return Ok(Value::Table(toml::Table::new()));
    }
    let table: toml::Table = std::fs::read_to_string(path)?.parse()?;
    tracing::info!(path = %path.display(), "已加载全局配置");
    Ok(Value::Table(table))
}
