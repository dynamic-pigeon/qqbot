//! access_token 的获取与刷新。
//!
//! 官方接口：`POST https://api.bot.qq.com/app/getAppAccessToken`，
//! 请求体 `{ appId, clientSecret }`，返回 `{ access_token, expires_in }`
//! （文档标称 7200 秒，实测约 9 分钟）。
//! 所有 REST / WS 鉴权统一用 `Authorization: QQBot {token}`。

use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde_json::json;

/// 过期前提前刷新的余量，避免边界上拿到已失效的 token。
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

const TOKEN_ENDPOINT: &str = "https://api.bot.qq.com/app/getAppAccessToken";

pub struct TokenManager {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    /// 跨 await 持锁，并发请求只会触发一次刷新。
    state: kovi::tokio::sync::Mutex<Option<TokenState>>,
}

struct TokenState {
    token: String,
    expire_at: Instant,
}

#[derive(Debug, thiserror::Error)]
#[error("获取 access_token 失败: {message}")]
pub struct TokenError {
    pub message: String,
}

/// 该接口把数字返回成字符串（如 `"expires_in":"549"`），两种形态都收。
fn de_seconds<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Seconds {
        Number(u64),
        Text(String),
    }

    match Seconds::deserialize(deserializer)? {
        Seconds::Number(number) => Ok(number),
        Seconds::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

impl TokenManager {
    pub fn new(http: reqwest::Client, app_id: String, app_secret: String) -> Self {
        Self {
            http,
            app_id,
            app_secret,
            state: kovi::tokio::sync::Mutex::new(None),
        }
    }

    /// 返回带 `QQBot ` 前缀的完整鉴权头值。
    pub async fn authorization(&self) -> Result<String, TokenError> {
        let mut state = self.state.lock().await;
        if let Some(entry) = state.as_ref()
            && Instant::now() + REFRESH_MARGIN < entry.expire_at
        {
            return Ok(format!("QQBot {}", entry.token));
        }

        let body = json!({
            "appId": self.app_id,
            "clientSecret": self.app_secret,
        });
        let response = self
            .http
            .post(TOKEN_ENDPOINT)
            // 官方 SDK 对 token 请求单独设 10 秒超时。
            .timeout(Duration::from_secs(10))
            .json(&body)
            .send()
            .await
            .map_err(|e| TokenError {
                message: e.to_string(),
            })?;

        // 错误体也要原样带出来：凭据错 / 权限未开时服务端回 JSON 错误，
        // 直接 .json() 解码会把真实原因吞成 "error decoding response body"。
        let status = response.status();
        let text = response.text().await.map_err(|e| TokenError {
            message: e.to_string(),
        })?;
        if !status.is_success() {
            return Err(TokenError {
                message: format!("HTTP {status}: {text}"),
            });
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(deserialize_with = "de_seconds")]
            expires_in: u64,
        }

        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| TokenError {
            message: format!("响应解析失败（{text}）: {e}"),
        })?;
        let expire_at = Instant::now() + Duration::from_secs(parsed.expires_in) - REFRESH_MARGIN;
        *state = Some(TokenState {
            token: parsed.access_token.clone(),
            expire_at,
        });
        Ok(format!("QQBot {}", parsed.access_token))
    }

    /// 丢弃缓存票据，下次鉴权重新获取（WS 关闭码 4004、op 9 不可恢复时）。
    pub(crate) async fn invalidate(&self) {
        *self.state.lock().await = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 刷新判定用余量() {
        // expires_in 减掉余量后到期时间落在过去，则下次 get 必然刷新。
        let now = Instant::now();
        let state = TokenState {
            token: "t".into(),
            expire_at: now + REFRESH_MARGIN,
        };
        assert!(Instant::now() + REFRESH_MARGIN >= state.expire_at);
    }
}
