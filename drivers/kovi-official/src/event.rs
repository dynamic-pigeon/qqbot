//! 官方事件类型、注册器与回复 API。

use std::future::Future;
use std::sync::Arc;

use kovi::PluginBuilder;
use kovi::bot::runtimebot::send_api_request_with_forget;
use kovi::bot::{BotInformation, SendApi};
use kovi::error::EventBuildError;
use kovi::event::id::ref_id::RefID;
use kovi::event::{Event, InternalEvent, MessageEventTrait};
use kovi::message::Message as KoviMessage;
use kovi::tokio::sync::mpsc;
use kovi::types::ApiAndOptOneshot;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct MsgEvent {
    pub time: i64,
    /// 群聊为 group_openid；单聊为 None。
    pub group_openid: Option<String>,
    /// 发送者：群内为 member_openid，单聊为 user_openid。
    pub user_openid: String,
    pub nickname: String,
    pub card: Option<String>,
    pub avatar: Option<String>,
    /// 官方消息 id，被动回复携带它。
    pub official_message_id: String,
    pub message: KoviMessage,
    /// 合并后的纯文本，纯图片消息为 None。
    pub text: Option<String>,
    pub(crate) api_tx: mpsc::Sender<ApiAndOptOneshot>,
}

impl MsgEvent {
    pub fn is_group(&self) -> bool {
        self.group_openid.is_some()
    }

    /// 事件归属会话（被动回复目标）。
    fn conversation(&self) -> (&'static str, &str) {
        match &self.group_openid {
            Some(openid) => ("group", openid),
            None => ("private", self.user_openid.as_str()),
        }
    }

    /// 回复触发本事件的消息（被动消息）；发送形态由 outbound 决定。
    pub fn reply<M: Into<KoviMessage>>(&self, msg: M) {
        let message = msg.into();
        let (scope, openid) = self.conversation();
        let api = SendApi::new(
            "official_send",
            json!({
                "scope": scope,
                "openid": openid,
                "reply_to": self.official_message_id,
                "message": segments_json(&message),
            }),
        );
        send_api_request_with_forget(&self.api_tx, api);
    }

    /// 以 markdown 回复；平台拒绝时驱动自动回退纯文本。
    pub fn reply_markdown(&self, content: &str) {
        self.reply(markdown_message(content));
    }

    /// 与 [`MsgEvent::reply`] 等价：官方的回复由 msg_id 绑定原消息，不另发
    /// message_reference。
    pub fn reply_and_quote<M: Into<KoviMessage>>(&self, msg: M) {
        self.reply(msg);
    }

    pub fn get_text(&self) -> &str {
        self.text.as_deref().unwrap_or_default()
    }
}

/// 消息段序列化：协议侧字段名是 `type`，kovi Segment 序列化输出的是
/// 结构体字段名 `kind`，出站必须在这里换名。
fn segments_json(message: &KoviMessage) -> Value {
    Value::Array(
        message
            .iter()
            .map(|segment| {
                json!({
                    "type": segment.kind,
                    "data": segment.data,
                })
            })
            .collect(),
    )
}

/// markdown 消息段：`{ type: "markdown", data: { content } }`。
pub fn markdown_message(content: &str) -> KoviMessage {
    KoviMessage::from(vec![kovi::message::Segment {
        kind: "markdown".to_owned(),
        data: json!({ "content": content }),
    }])
}

impl MessageEventTrait for MsgEvent {
    fn get_sender_name(&self) -> Option<&str> {
        Some(&self.nickname)
    }

    fn get_message(&self) -> &KoviMessage {
        &self.message
    }

    fn get_message_type_str(&self) -> Option<&str> {
        Some(match self.group_openid {
            Some(_) => "group",
            None => "private",
        })
    }

    fn get_sender_id(&self) -> RefID<'_> {
        RefID::new(&self.user_openid)
    }

    fn get_group_id(&self) -> Option<RefID<'_>> {
        self.group_openid.as_ref().map(RefID::new)
    }
}

impl Event for MsgEvent {
    fn de(
        event: &InternalEvent,
        _: &BotInformation,
        api_tx: &mpsc::Sender<ApiAndOptOneshot>,
    ) -> Option<Self> {
        let InternalEvent::DriverEvent(json) = event else {
            return None;
        };
        parse_event(json, api_tx.clone()).ok()
    }
}

fn parse_event(
    json: &Value,
    api_tx: mpsc::Sender<ApiAndOptOneshot>,
) -> Result<MsgEvent, EventBuildError> {
    let field = |name: &str| {
        json.get(name)
            .and_then(Value::as_str)
            .ok_or(EventBuildError::ParseError(format!("缺少字段 {name}")))
    };

    let message = serde_json::from_value::<Vec<kovi::message::Segment>>(
        json.get("message").cloned().unwrap_or_else(|| json!([])),
    )
    .map_err(|e| EventBuildError::ParseError(e.to_string()))?;
    let message = KoviMessage::from(message);
    let text = {
        let parts: Vec<&str> = message
            .iter()
            .filter(|segment| segment.kind == "text")
            .filter_map(|segment| segment.data.get("text").and_then(Value::as_str))
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    };

    Ok(MsgEvent {
        time: json.get("time").and_then(Value::as_i64).unwrap_or_default(),
        group_openid: json
            .get("group_openid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        user_openid: field("user_openid")?.to_owned(),
        nickname: field("nickname")?.to_owned(),
        card: json.get("card").and_then(Value::as_str).map(str::to_owned),
        avatar: json
            .get("avatar")
            .and_then(Value::as_str)
            .map(str::to_owned),
        official_message_id: field("message_id")?.to_owned(),
        text,
        message,
        api_tx,
    })
}

/// 群消息事件：`on_group_msg` 用。
#[derive(Debug, Clone)]
pub struct GroupMsgEvent(pub Arc<MsgEvent>);

impl std::ops::Deref for GroupMsgEvent {
    type Target = MsgEvent;
    fn deref(&self) -> &MsgEvent {
        &self.0
    }
}

impl Event for GroupMsgEvent {
    fn de(
        event: &InternalEvent,
        bot_info: &BotInformation,
        api_tx: &mpsc::Sender<ApiAndOptOneshot>,
    ) -> Option<Self> {
        let inner = MsgEvent::de(event, bot_info, api_tx)?;
        inner.is_group().then(|| GroupMsgEvent(Arc::new(inner)))
    }
}

impl MessageEventTrait for GroupMsgEvent {
    fn get_sender_name(&self) -> Option<&str> {
        self.0.get_sender_name()
    }
    fn get_message(&self) -> &KoviMessage {
        self.0.get_message()
    }
    fn get_message_type_str(&self) -> Option<&str> {
        self.0.get_message_type_str()
    }
    fn get_sender_id(&self) -> RefID<'_> {
        self.0.get_sender_id()
    }
    fn get_group_id(&self) -> Option<RefID<'_>> {
        self.0.get_group_id()
    }
}

/// 单聊消息事件：`on_private_msg` 用。
#[derive(Debug, Clone)]
pub struct PrivateMsgEvent(pub Arc<MsgEvent>);

impl std::ops::Deref for PrivateMsgEvent {
    type Target = MsgEvent;
    fn deref(&self) -> &MsgEvent {
        &self.0
    }
}

impl Event for PrivateMsgEvent {
    fn de(
        event: &InternalEvent,
        bot_info: &BotInformation,
        api_tx: &mpsc::Sender<ApiAndOptOneshot>,
    ) -> Option<Self> {
        let inner = MsgEvent::de(event, bot_info, api_tx)?;
        (!inner.is_group()).then(|| PrivateMsgEvent(Arc::new(inner)))
    }
}

impl MessageEventTrait for PrivateMsgEvent {
    fn get_sender_name(&self) -> Option<&str> {
        self.0.get_sender_name()
    }
    fn get_message(&self) -> &KoviMessage {
        self.0.get_message()
    }
    fn get_message_type_str(&self) -> Option<&str> {
        self.0.get_message_type_str()
    }
    fn get_sender_id(&self) -> RefID<'_> {
        self.0.get_sender_id()
    }
    fn get_group_id(&self) -> Option<RefID<'_>> {
        self.0.get_group_id()
    }
}

/// 事件注册器，方法名与 kovi-onebot 对齐，插件换 import 即可。
pub trait EventRegistrar {
    fn on_msg<F, Fut>(handler: F)
    where
        F: Fn(Arc<MsgEvent>) -> Fut + Send + Sync + 'static,
        Fut: Future + Send,
        Fut::Output: Send,
    {
        PluginBuilder::on::<MsgEvent, _>(handler)
    }

    fn on_group_msg<F, Fut>(handler: F)
    where
        F: Fn(Arc<GroupMsgEvent>) -> Fut + Send + Sync + 'static,
        Fut: Future + Send,
        Fut::Output: Send,
    {
        PluginBuilder::on::<GroupMsgEvent, _>(handler)
    }

    fn on_private_msg<F, Fut>(handler: F)
    where
        F: Fn(Arc<PrivateMsgEvent>) -> Fut + Send + Sync + 'static,
        Fut: Future + Send,
        Fut::Output: Send,
    {
        PluginBuilder::on::<PrivateMsgEvent, _>(handler)
    }
}

impl EventRegistrar for PluginBuilder {}
