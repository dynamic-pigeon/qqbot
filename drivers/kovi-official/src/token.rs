//! access_token 的获取与刷新。
//!
//! `POST /app/getAppAccessToken`，请求体 `{ appId, clientSecret }`，返回
//! `{ access_token, expires_in }`（文档标称 7200 秒，实测约 9 分钟）。
//! 所有 REST / WS 鉴权统一用 `Authorization: QQBot {token}`。

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::de_u64;

const TOKEN_ENDPOINT: &str = "https://api.bot.qq.com/app/getAppAccessToken";
/// token 请求单独设 10 秒超时。
const TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
/// 过期前提前刷新的余量，避免边界上拿到已失效的 token。
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(deserialize_with = "de_u64")]
    expires_in: u64,
}

struct TokenState {
    token: String,
    expire_at: Instant,
}

impl TokenState {
    /// 票据是否会在 `margin` 之内过期。
    fn expires_within(&self, margin: Duration) -> bool {
        Instant::now() + margin >= self.expire_at
    }
}

pub struct TokenManager {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    /// 跨 await 持锁，并发请求只会触发一次刷新。
    state: kovi::tokio::sync::Mutex<Option<TokenState>>,
}

#[derive(Debug, thiserror::Error)]
#[error("获取 access_token 失败: {message}")]
pub struct TokenError {
    pub message: String,
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
            && !entry.expires_within(REFRESH_MARGIN)
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
            .timeout(TOKEN_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| TokenError {
                message: e.to_string(),
            })?;

        // 错误体原样带出：凭据错 / 权限未开时服务端回 JSON 错误，直接解码
        // 会把真实原因吞成 "error decoding response body"。
        let status = response.status();
        let text = response.text().await.map_err(|e| TokenError {
            message: e.to_string(),
        })?;
        if !status.is_success() {
            return Err(TokenError {
                message: format!("HTTP {status}: {text}"),
            });
        }

        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| TokenError {
            message: format!("响应解析失败（{text}）: {e}"),
        })?;
        let token = TokenState {
            token: parsed.access_token.clone(),
            expire_at: Instant::now() + Duration::from_secs(parsed.expires_in),
        };
        *state = Some(token);
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
    fn 落在刷新余量内的票据视为过期() {
        let state = |expire_at| TokenState {
            token: "t".into(),
            expire_at,
        };
        let fresh = state(Instant::now() + REFRESH_MARGIN + Duration::from_secs(1));
        let stale = state(Instant::now() + REFRESH_MARGIN);
        assert!(!fresh.expires_within(REFRESH_MARGIN));
        assert!(stale.expires_within(REFRESH_MARGIN));
    }
}
