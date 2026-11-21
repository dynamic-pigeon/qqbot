//! WS 网关客户端：接入点解析、鉴权、心跳、断线重连（resume）。
//!
//! 帧协议是 Discord 风格的 opcode：
//! 0 Dispatch（事件）/ 1 Heartbeat / 2 Identify / 6 Resume /
//! 7 Reconnect / 9 Invalid Session / 10 Hello / 11 Heartbeat ACK。

use std::sync::Arc;
use std::time::Duration;

use kovi::futures_util::stream::SplitStream;
use kovi::futures_util::{SinkExt, StreamExt};
use kovi::tokio::sync::mpsc;
use kovi::tokio::sync::oneshot;
use serde_json::{Value, json};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, connect_async};

use kovi::driver::{AnyError, DriverEvent};

use crate::Context;

/// C2C_MESSAGE_CREATE + GROUP_AT_MESSAGE_CREATE（含开启全量后的
/// GROUP_MESSAGE_CREATE）都在这一个订阅位下。
const INTENT_GROUP_AND_C2C: u64 = 1 << 25;

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub last_seq: u64,
}

pub async fn run(
    ctx: Arc<Context>,
    events: mpsc::Sender<Result<DriverEvent, AnyError>>,
    ready: oneshot::Sender<()>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut ready_signal = Some(ready);
    loop {
        let outcome = connection(&ctx, &events, ready_signal.take()).await;
        if outcome.was_ready {
            // 例行断开（平台约一小时一轮换），立即重连。
            tracing::debug!("官方网关例行断开: {}", outcome.error);
            backoff = Duration::from_secs(1);
        } else {
            tracing::warn!(
                "官方网关连接失败: {}，{} 秒后重连",
                outcome.error,
                backoff.as_secs()
            );
            kovi::tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }
}

struct Outcome {
    /// 本次连接是否到达过 READY / RESUMED（到达过则重连不退避）。
    was_ready: bool,
    error: String,
}

async fn connection(
    ctx: &Arc<Context>,
    events: &mpsc::Sender<Result<DriverEvent, AnyError>>,
    mut ready: Option<oneshot::Sender<()>>,
) -> Outcome {
    let endpoint = resolve_endpoint(ctx).await;

    let request = match endpoint.as_str().into_client_request() {
        Ok(request) => request,
        Err(e) => {
            return Outcome {
                was_ready: false,
                error: format!("接入点非法（{endpoint}）: {e}"),
            };
        }
    };

    let websocket = match connect_async(request).await {
        Ok((stream, _)) => stream,
        Err(e) => {
            return Outcome {
                was_ready: false,
                error: format!("连接失败（{endpoint}）: {e}"),
            };
        }
    };
    let (mut writer, mut reader) = websocket.split();

    // 建连首帧必须是 op=10 Hello，心跳间隔由此确定。
    let heartbeat_interval = match expect_hello(&mut reader).await {
        Ok(interval) => interval,
        Err(e) => {
            return Outcome {
                was_ready: false,
                error: e,
            };
        }
    };

    let authorization = match ctx.token.authorization().await {
        Ok(token) => token,
        Err(e) => {
            return Outcome {
                was_ready: false,
                error: e.to_string(),
            };
        }
    };

    // 有存活的会话就 resume，否则 identify。
    let existing = ctx
        .session
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let identify_frame = match &existing {
        Some(session) => json!({
            "op": 6,
            "d": {
                "token": authorization,
                "session_id": session.id,
                "seq": session.last_seq,
            }
        }),
        None => json!({
            "op": 2,
            "d": {
                "token": authorization,
                "intents": INTENT_GROUP_AND_C2C,
                "shard": [0, 1],
                "properties": {},
            }
        }),
    };
    if let Err(e) = writer.send(Message::text(identify_frame.to_string())).await {
        return Outcome {
            was_ready: false,
            error: format!("发送鉴权帧失败: {e}"),
        };
    }

    let mut was_ready = false;
    let mut heartbeat = kovi::tokio::time::interval(heartbeat_interval);
    heartbeat.set_missed_tick_behavior(kovi::tokio::time::MissedTickBehavior::Delay);
    // 心跳间隔的三倍没有读到任何帧，视为连接僵死。
    let read_timeout = heartbeat_interval * 3;

    loop {
        kovi::tokio::select! {
            _ = heartbeat.tick() => {
                let seq = ctx
                    .session
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .map(|session| session.last_seq)
                    .unwrap_or_default();
                let payload = if seq == 0 {
                    json!({ "op": 1, "d": null })
                } else {
                    json!({ "op": 1, "d": seq })
                };
                if let Err(e) = writer.send(Message::text(payload.to_string())).await {
                    return Outcome { was_ready, error: format!("心跳发送失败: {e}") };
                }
            }
            frame = kovi::tokio::time::timeout(read_timeout, reader.next()) => {
                let message = match frame {
                    Err(_) => {
                        return Outcome { was_ready, error: "读超时（连接僵死）".into() }
                    }
                    Ok(None) => return Outcome { was_ready, error: "连接关闭".into() },
                    Ok(Some(Err(e))) => {
                        return Outcome { was_ready, error: format!("读取失败: {e}") }
                    }
                    Ok(Some(Ok(message))) => message,
                };
                let Message::Text(text) = message else { continue };
                match dispatch_frame(ctx, events, &mut ready, &mut was_ready, &text).await {
                    FrameAction::Continue => {}
                    FrameAction::Reconnect => {
                        return Outcome { was_ready, error: "服务端要求重连".into() }
                    }
                    FrameAction::DropSession => {
                        *ctx.session.lock().unwrap_or_else(|p| p.into_inner()) = None;
                        return Outcome { was_ready, error: "会话失效（op 9），需重新鉴权".into() }
                    }
                }
            }
        }
    }
}

enum FrameAction {
    Continue,
    Reconnect,
    DropSession,
}

async fn dispatch_frame(
    ctx: &Arc<Context>,
    events: &mpsc::Sender<Result<DriverEvent, AnyError>>,
    ready: &mut Option<oneshot::Sender<()>>,
    was_ready: &mut bool,
    text: &str,
) -> FrameAction {
    let frame: Value = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(e) => {
            tracing::warn!("网关帧解析失败: {e}");
            return FrameAction::Continue;
        }
    };

    match frame["op"].as_i64().unwrap_or(-1) {
        0 => {
            let event_type = frame["t"].as_str().unwrap_or_default();
            record_seq(ctx, &frame);
            match event_type {
                "READY" => {
                    let payload = &frame["d"];
                    let mut session = ctx.session.lock().unwrap_or_else(|p| p.into_inner());
                    let last_seq = session.as_ref().map(|s| s.last_seq).unwrap_or_default();
                    *session = Some(Session {
                        id: payload["session_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        last_seq,
                    });
                    drop(session);
                    tracing::info!(
                        bot = payload["user"]["username"].as_str().unwrap_or_default(),
                        "官方网关已就绪"
                    );
                    if let Some(signal) = ready.take() {
                        let _ = signal.send(());
                    }
                    *was_ready = true;
                }
                "RESUMED" => {
                    tracing::debug!("官方网关会话已恢复");
                    *was_ready = true;
                }
                "GROUP_AT_MESSAGE_CREATE" | "GROUP_MESSAGE_CREATE" | "C2C_MESSAGE_CREATE" => {
                    if let Some(official_event) =
                        crate::inbound::translate(ctx, event_type, &frame["d"])
                    {
                        // 下游已关闭时发送失败，此时事件丢弃即可。
                        let _ = events.send(Ok(DriverEvent::Normal(official_event))).await;
                    }
                }
                _ => {}
            }
        }
        7 => return FrameAction::Reconnect,
        9 => return FrameAction::DropSession,
        _ => {}
    }
    FrameAction::Continue
}

fn record_seq(ctx: &Arc<Context>, frame: &Value) {
    let Some(seq) = frame["s"].as_u64() else {
        return;
    };
    let mut session = ctx.session.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(session) = session.as_mut() {
        session.last_seq = session.last_seq.max(seq);
    }
}

async fn expect_hello(
    reader: &mut SplitStream<WebSocketStream<MaybeTlsStream<kovi::tokio::net::TcpStream>>>,
) -> Result<Duration, String> {
    let message = reader
        .next()
        .await
        .ok_or_else(|| "建连后连接立即关闭".to_owned())?
        .map_err(|e| format!("读取 Hello 失败: {e}"))?;
    let Message::Text(text) = message else {
        return Err("建连首帧不是文本".into());
    };
    let frame: Value = serde_json::from_str(&text).map_err(|e| format!("Hello 解析失败: {e}"))?;
    if frame["op"] != 10 {
        return Err(format!("建连首帧 op 不是 10: {text}"));
    }
    let millis = frame["d"]["heartbeat_interval"]
        .as_u64()
        .ok_or_else(|| "Hello 缺少 heartbeat_interval".to_owned())?;
    Ok(Duration::from_millis(millis.max(1000)))
}

/// 官方推荐先调 `GET /websocket` 拿接入点；失败时退回配置或默认地址。
async fn resolve_endpoint(ctx: &Arc<Context>) -> String {
    if let Some(url) = &ctx.config.ws_url {
        return url.clone();
    }
    let endpoint = match ctx.token.authorization().await {
        Ok(authorization) => match ctx
            .http
            .get(format!("{}/websocket", ctx.config.api_base()))
            .header("Authorization", authorization)
            .send()
            .await
        {
            Ok(response) => match response.json::<Value>().await {
                Ok(body) => body["url"].as_str().map(str::to_owned),
                Err(_) => None,
            },
            Err(_) => None,
        },
        Err(_) => None,
    };
    match endpoint {
        Some(url) => url,
        None => {
            tracing::debug!("接入点探测失败，使用默认地址");
            ctx.config.fallback_ws_url().to_owned()
        }
    }
}
