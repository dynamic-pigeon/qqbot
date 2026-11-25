//! `kovi.conf.toml` 的 `[official]` 段。

use std::fs;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OfficialDriverConfig {
    pub app_id: String,
    pub app_secret: String,
    #[serde(default)]
    pub sandbox: bool,
    pub ws_url: Option<String>,
}

impl OfficialDriverConfig {
    /// REST 请求基地址；沙箱与正式环境分开。
    pub fn api_base(&self) -> &'static str {
        if self.sandbox {
            "https://sandbox.api.sgroup.qq.com"
        } else {
            "https://api.bot.qq.com"
        }
    }

    /// WS 接入点探测失败时的兜底地址。
    pub fn fallback_ws_url(&self) -> &'static str {
        if self.sandbox {
            "wss://sandbox.api.sgroup.qq.com/websocket"
        } else {
            "wss://api.bot.qq.com/websocket"
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取 kovi.conf.toml 失败: {0}")]
    Read(String),
    #[error("kovi.conf.toml 缺少 [official] 段，需要 app_id / app_secret")]
    MissingSection,
    #[error("kovi.conf.toml [official] 段解析失败: {0}")]
    Parse(String),
    #[error("kovi.conf.toml [official] 段的 app_id / app_secret 不能为空")]
    Empty,
}

#[derive(serde::Deserialize)]
struct ConfFile {
    official: Option<OfficialDriverConfig>,
}

fn parse_conf(raw: &str) -> Result<OfficialDriverConfig, ConfigError> {
    let file: ConfFile = toml::from_str(raw).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let config = file.official.ok_or(ConfigError::MissingSection)?;
    if config.app_id.trim().is_empty() || config.app_secret.trim().is_empty() {
        return Err(ConfigError::Empty);
    }
    Ok(config)
}

/// 从 `kovi.conf.toml` 读取 `[official]` 段。
pub fn load_local_conf() -> Result<OfficialDriverConfig, ConfigError> {
    let raw = fs::read_to_string(Path::new("kovi.conf.toml"))
        .map_err(|e| ConfigError::Read(e.to_string()))?;
    parse_conf(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只取official段并按沙箱切基地址() {
        let raw = "[server]\nport = 8080\n\n[official]\napp_id = \"1020\"\napp_secret = \"s\"\n";
        let config = parse_conf(raw).unwrap();
        assert_eq!(config.app_id, "1020");
        assert_eq!(config.api_base(), "https://api.bot.qq.com");

        let raw = "[official]\napp_id = \"1\"\napp_secret = \"s\"\nsandbox = true\n";
        let sandbox = parse_conf(raw).unwrap();
        assert_eq!(sandbox.api_base(), "https://sandbox.api.sgroup.qq.com");
    }

    #[test]
    fn 缺段或凭据为空报错() {
        let missing = parse_conf("[config]\nmain_admin = 1\n");
        let empty = parse_conf("[official]\napp_id = \" \"\napp_secret = \"s\"\n");
        assert!(matches!(missing, Err(ConfigError::MissingSection)));
        assert!(matches!(empty, Err(ConfigError::Empty)));
    }
}
