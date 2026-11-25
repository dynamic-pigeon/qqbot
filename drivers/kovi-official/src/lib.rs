//! kovi 的 QQ 官方机器人驱动。
//!
//! 身份自始至终是 openid 字符串：事件类型（[`event::MsgEvent`]）、
//! 发送 action、成员缓存、被动回复路由全部按字符串流转，不做数字映射。
//! 插件通过 utils 的门面使用，不直接感知协议。

pub mod event;

mod config;
mod gateway;
mod inbound;
mod media;
mod member_cache;
mod outbound;
mod passive;
mod rate_limit;
mod token;

pub use config::{ConfigError, OfficialDriverConfig, load_local_conf};

use std::sync::Arc;
use std::time::Duration;

use kovi::ApiReturn;
use kovi::bot::SendApi;
use kovi::driver::{AnyError, ApiHandlerResult, Driver, DriverEvent, MessageEventRegister};
use kovi::futures_util::Stream;
use kovi::tokio::sync::{mpsc, oneshot};
use serde::Deserialize;
use serde_json::Value;

/// REST 请求超时。
const REST_TIMEOUT: Duration = Duration::from_secs(30);

/// 上传类请求超时。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// 发送目标会话：群按 `group_openid`，单聊按 `user_openid`。
enum ChatScope {
    Group(String),
    User(String),
}

impl ChatScope {
    /// REST 路径前缀：`/v2/groups/{group_openid}` 或 `/v2/users/{openid}`。
    fn base_path(&self) -> String {
        match self {
            ChatScope::Group(openid) => format!("/v2/groups/{openid}"),
            ChatScope::User(openid) => format!("/v2/users/{openid}"),
        }
    }

    /// 会话类型标签：群 `g`，单聊 `u`。
    fn kind(&self) -> &'static str {
        match self {
            ChatScope::Group(_) => "g",
            ChatScope::User(_) => "u",
        }
    }

    /// 被动回复槽位与发送节流共用的会话键。
    fn key(&self) -> String {
        let openid = match self {
            ChatScope::Group(openid) | ChatScope::User(openid) => openid,
        };
        format!("{}:{openid}", self.kind())
    }

    /// 被动回复有效窗口：群 5 分钟，单聊 60 分钟。
    fn passive_window(&self) -> Duration {
        match self {
            ChatScope::Group(_) => Duration::from_secs(5 * 60),
            ChatScope::User(_) => Duration::from_secs(60 * 60),
        }
    }
}

/// POST 失败：传输出错，或平台返回非 2xx。
#[derive(Debug, thiserror::Error)]
enum PostError {
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error("HTTP {status}: {body}")]
    Rejected {
        status: reqwest::StatusCode,
        body: String,
    },
}

impl PostError {
    /// 响应体里的业务错误码（`err_code` 或 `code`）。
    fn biz_code(&self) -> Option<u64> {
        let PostError::Rejected { body, .. } = self else {
            return None;
        };
        let body: Value = serde_json::from_str(body).ok()?;
        ["err_code", "code"].into_iter().find_map(|key| {
            body[key]
                .as_u64()
                .or_else(|| body[key].as_str()?.parse().ok())
        })
    }
}

/// 带 Authorization 的 JSON POST；成功返回响应体文本。
async fn post_json(
    http: &reqwest::Client,
    authorization: &str,
    url: String,
    body: &Value,
    timeout: Duration,
) -> Result<String, PostError> {
    let response = http
        .post(url)
        .timeout(timeout)
        .header("Authorization", authorization)
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if status.is_success() {
        Ok(text)
    } else {
        Err(PostError::Rejected { status, body: text })
    }
}

/// 官方接口会把数字返回成字符串，u64 字段两种形态都收。
fn de_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::Number(number) => Ok(number),
        StringOrNumber::Text(text) => text.parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrNumber {
    Number(u64),
    Text(String),
}

/// 驱动内各模块共享的状态。
struct Context {
    config: OfficialDriverConfig,
    http: reqwest::Client,
    token: token::TokenManager,
    members: member_cache::MemberCache,
    media: media::MediaStore,
    limiter: rate_limit::SendRateLimiter,
    inbound: passive::RecentInbound,
    seen: inbound::SeenMessages,
    session: std::sync::Mutex<Option<gateway::Session>>,
}

impl Context {
    #[cfg(test)]
    fn for_tests() -> Self {
        Self::build(OfficialDriverConfig {
            app_id: "test".to_owned(),
            app_secret: "test".to_owned(),
            sandbox: false,
            ws_url: None,
        })
    }

    fn build(config: OfficialDriverConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REST_TIMEOUT)
            .build()
            .expect("固定配置的 reqwest client 必然合法");
        let token = token::TokenManager::new(
            http.clone(),
            config.app_id.clone(),
            config.app_secret.clone(),
        );
        Self {
            token,
            http,
            config,
            members: member_cache::MemberCache::new(),
            media: media::MediaStore::new(),
            // 官方频控：单会话每分钟 20 条（群与单聊同标准）。
            limiter: rate_limit::SendRateLimiter::new(Duration::from_secs(60), 20),
            inbound: passive::RecentInbound::new(),
            seen: inbound::SeenMessages::new(),
            session: std::sync::Mutex::new(None),
        }
    }
}

pub struct OfficialDriver {
    ctx: Arc<Context>,
}

impl OfficialDriver {
    pub fn new(config: OfficialDriverConfig) -> Self {
        Self {
            ctx: Arc::new(Context::build(config)),
        }
    }
}

#[async_trait::async_trait]
impl Driver for OfficialDriver {
    async fn event_channel(
        &self,
    ) -> Result<std::pin::Pin<Box<dyn Stream<Item = Result<DriverEvent, AnyError>> + Send>>, AnyError>
    {
        let (events, events_rx) = mpsc::channel(64);
        let (ready_tx, ready_rx) = oneshot::channel();
        kovi::tokio::spawn(gateway::run(Arc::clone(&self.ctx), events, ready_tx));

        // 坏凭据 / 网络不通在启动期暴露，不在运行中期才发现。
        match kovi::tokio::time::timeout(Duration::from_secs(30), ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err("官方网关任务异常退出".into()),
            Err(_) => {
                return Err(
                    "等待官方网关 READY 超时（30 秒），请检查 kovi.conf.toml 的 [official] 段"
                        .into(),
                );
            }
        }

        let stream = kovi::futures_util::stream::unfold(events_rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(Box::pin(stream))
    }

    fn api_handler(&self, value: SendApi) -> ApiHandlerResult {
        let ctx = Arc::clone(&self.ctx);
        Box::pin(async move {
            let result = outbound::handle_action(&ctx, &value.action, value.params).await;
            Ok(match result {
                Ok(data) => Ok(ApiReturn {
                    status: "ok".to_owned(),
                    retcode: 0,
                    message: None,
                    data,
                }),
                Err(e) => {
                    tracing::warn!(
                        action = %value.action,
                        retcode = e.retcode,
                        message = %e.message,
                        "官方 action 失败"
                    );
                    Err(ApiReturn {
                        status: "failed".to_owned(),
                        retcode: e.retcode,
                        message: Some(e.message),
                        data: serde_json::Value::Null,
                    })
                }
            })
        })
    }

    fn message_event_register(&self) -> MessageEventRegister {
        MessageEventRegister::register::<event::MsgEvent>()
    }
}
