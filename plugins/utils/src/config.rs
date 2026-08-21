//! 当前目录 `config.toml`。读成 TOML [`Value`]，各插件用 [`parse`] 反序列化自己的表。

use std::sync::LazyLock;

use serde::de::DeserializeOwned;

pub use toml::Value;

/// 根目录 `config.toml`。找不到文件时为空表。
pub fn value() -> &'static Value {
    static CONFIG: LazyLock<Value> =
        LazyLock::new(|| load().unwrap_or_else(|error| panic!("加载全局配置失败: {error:#}")));
    &CONFIG
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
