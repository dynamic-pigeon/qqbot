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

/// 驱动内各模块共享的状态。
pub(crate) struct Context {
    pub(crate) config: OfficialDriverConfig,
    pub(crate) http: reqwest::Client,
    pub(crate) token: token::TokenManager,
    pub(crate) members: member_cache::MemberCache,
    pub(crate) media: media::MediaStore,
    pub(crate) limiter: rate_limit::SendRateLimiter,
    pub(crate) inbound: passive::RecentInbound,
    pub(crate) seen: inbound::SeenMessages,
    pub(crate) session: std::sync::Mutex<Option<gateway::Session>>,
}

impl Context {
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self::build(OfficialDriverConfig {
            app_id: "test".to_owned(),
            app_secret: "test".to_owned(),
            sandbox: false,
            ws_url: None,
        })
    }

    fn build(config: OfficialDriverConfig) -> Self {
        // 官方 SDK 口径：REST 默认 30 秒超时，上传类请求单独放宽（media.rs）。
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
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
