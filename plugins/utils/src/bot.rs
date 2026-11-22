//! 两栈统一的机器人门面。
//!
//! `official` feature（由根 crate 的 `official` 联动开启）装配 kovi-official：
//! 身份是 openid 字符串。默认栈装配 kovi-onebot：身份是 QQ 号字符串。
//! 插件只 import 本模块，事件、注册、回复、发送、消息构建全部经由这里的
//! 自有类型，不接触任何驱动 crate。

use std::future::Future;
use std::sync::Arc;

use kovi::Message;
use kovi::RuntimeBot;

/// 机器人驱动栈。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    Official,
    OneBot,
}

/// 当前装配的驱动栈。
pub fn stack() -> Stack {
    #[cfg(feature = "official")]
    {
        Stack::Official
    }
    #[cfg(not(feature = "official"))]
    {
        Stack::OneBot
    }
}

// ── 消息构建 ──────────────────────────────────────────────────────────────

/// 消息段构建：两栈共用同一消息形状（kovi::Message 的 text / image 段）。
pub trait MessageExt: Sized {
    fn add_text<T: std::fmt::Display>(self, text: T) -> Self;
    fn add_image(self, file: &str) -> Self;
    /// @ 人：官方栈 id 是 openid，OneBot 栈是 QQ 号十进制串。
    fn add_at(self, id: &str) -> Self;
    /// 引用回复：id 形态由栈决定（官方 ref_msg_idx 串 / OneBot 消息 id）。
    fn add_reply(self, id: &str) -> Self;
    fn push_image(&mut self, file: &str);
}

impl MessageExt for Message {
    fn add_text<T: std::fmt::Display>(mut self, text: T) -> Self {
        self.push(kovi::message::Segment {
            kind: "text".to_owned(),
            data: kovi::serde_json::json!({ "text": text.to_string() }),
        });
        self
    }

    fn add_image(mut self, file: &str) -> Self {
        self.push_image(file);
        self
    }

    fn add_at(mut self, id: &str) -> Self {
        self.push(kovi::message::Segment {
            kind: "at".to_owned(),
            data: kovi::serde_json::json!({ "qq": id }),
        });
        self
    }

    fn add_reply(mut self, id: &str) -> Self {
        self.push(kovi::message::Segment {
            kind: "reply".to_owned(),
            data: kovi::serde_json::json!({ "id": id }),
        });
        self
    }

    fn push_image(&mut self, file: &str) {
        self.push(kovi::message::Segment {
            kind: "image".to_owned(),
            data: kovi::serde_json::json!({ "file": file }),
        });
    }
}

/// 消息段序列化成 OneBot 形状（字段名 `type`）。kovi 的 Segment 序列化
/// 用 `kind` 字段名，协议端（含合并转发节点）要 `type`。
pub fn message_segments_json(message: &Message) -> kovi::serde_json::Value {
    kovi::serde_json::Value::Array(
        message
            .iter()
            .map(|segment| {
                kovi::serde_json::json!({
                    "type": segment.kind,
                    "data": segment.data,
                })
            })
            .collect(),
    )
}

/// bot 自身身份键（转发节点展示名下的小号）。官方栈无合并转发，占位返回。
#[cfg_attr(feature = "official", allow(unused_variables))]
pub async fn bot_key(bot: &RuntimeBot) -> String {
    #[cfg(not(feature = "official"))]
    {
        use kovi_onebot::OnebotTrait as _;
        bot.get_login_info()
            .await
            .ok()
            .and_then(|response| {
                response
                    .data
                    .get("user_id")
                    .and_then(kovi::serde_json::Value::as_i64)
            })
            .map(|user_id| user_id.to_string())
            .unwrap_or_default()
    }
    #[cfg(feature = "official")]
    {
        "0".to_owned()
    }
}

// ── 事件门面 ──────────────────────────────────────────────────────────────

/// 事件门面：身份统一为字符串键，回复统一为消息段。
pub trait BotMsgEvent: Send + Sync {
    /// 发送者身份键：官方栈为 openid，OneBot 栈为 QQ 号十进制字符串。
    fn user_key(&self) -> String;
    /// 群身份键；私聊为 None。
    fn group_key(&self) -> Option<String>;
    fn nickname(&self) -> &str;
    /// 群名片；无名片或未知为 None。
    fn card(&self) -> Option<&str>;
    /// 头像 URL：官方栈来自事件（带时效，取用要及时），OneBot 栈按 QQ 号拼 qlogo。
    fn avatar_url(&self) -> Option<String>;
    fn message(&self) -> &Message;
    /// 合并后的纯文本；纯图片消息为 None。
    fn text(&self) -> Option<&str>;
    fn is_group(&self) -> bool;
    /// 事件时刻（unix 秒）。
    fn timestamp(&self) -> i64;
    fn reply(&self, message: Message);
    fn reply_and_quote(&self, message: Message);
    /// markdown 回复：官方栈走 msg_type=2（平台拒绝时驱动自动回退纯文本），
    /// OneBot 栈等价于纯文本回复。
    fn reply_markdown(&self, content: &str) {
        self.reply(Message::from(content));
    }
}

/// kovi-onebot 的三个事件结构字段同构，用宏统一挂门面。
#[cfg(not(feature = "official"))]
macro_rules! impl_bot_msg_event_for_onebot {
    ($event:ty, $group_key:expr) => {
        impl BotMsgEvent for $event {
            fn user_key(&self) -> String {
                self.user_id.to_string()
            }

            fn group_key(&self) -> Option<String> {
                $group_key(self)
            }

            fn nickname(&self) -> &str {
                self.sender.nickname.as_deref().unwrap_or_default()
            }

            fn card(&self) -> Option<&str> {
                self.sender.card.as_deref()
            }

            fn avatar_url(&self) -> Option<String> {
                Some(format!(
                    "https://q4.qlogo.cn/headimg_dl?dst_uin={}",
                    self.user_id
                ))
            }

            fn message(&self) -> &Message {
                &self.message
            }

            fn text(&self) -> Option<&str> {
                self.text.as_deref()
            }

            fn is_group(&self) -> bool {
                self.message_type == "group"
            }

            fn timestamp(&self) -> i64 {
                self.time
            }

            fn reply(&self, message: Message) {
                kovi_onebot::RepliableEvent::reply(self, message);
            }

            fn reply_and_quote(&self, message: Message) {
                kovi_onebot::RepliableEvent::reply_and_quote(self, message);
            }
        }
    };
}

#[cfg(not(feature = "official"))]
impl_bot_msg_event_for_onebot!(kovi_onebot::MsgEvent, |event: &kovi_onebot::MsgEvent| event
    .group_id
    .map(|group| group.to_string()));
#[cfg(not(feature = "official"))]
impl_bot_msg_event_for_onebot!(
    kovi_onebot::GroupMsgEvent,
    |event: &kovi_onebot::GroupMsgEvent| { Some(event.group_id.to_string()) }
);
#[cfg(not(feature = "official"))]
impl_bot_msg_event_for_onebot!(
    kovi_onebot::PrivateMsgEvent,
    |_: &kovi_onebot::PrivateMsgEvent| { None }
);

#[cfg(feature = "official")]
impl BotMsgEvent for kovi_official::event::MsgEvent {
    fn user_key(&self) -> String {
        self.user_openid.clone()
    }

    fn group_key(&self) -> Option<String> {
        self.group_openid.clone()
    }

    fn nickname(&self) -> &str {
        &self.nickname
    }

    fn card(&self) -> Option<&str> {
        self.card.as_deref()
    }

    fn avatar_url(&self) -> Option<String> {
        self.avatar.clone()
    }

    fn message(&self) -> &Message {
        &self.message
    }

    fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    fn is_group(&self) -> bool {
        self.is_group()
    }

    fn timestamp(&self) -> i64 {
        self.time
    }

    fn reply(&self, message: Message) {
        kovi_official::event::MsgEvent::reply(self, message);
    }

    fn reply_and_quote(&self, message: Message) {
        kovi_official::event::MsgEvent::reply_and_quote(self, message);
    }

    fn reply_markdown(&self, content: &str) {
        kovi_official::event::MsgEvent::reply_markdown(self, content);
    }
}

// ── 事件注册 ──────────────────────────────────────────────────────────────

/// 监听全部消息事件（群 + 私聊）。
pub fn on_msg<F, Fut>(handler: F)
where
    F: Fn(Arc<dyn BotMsgEvent>) -> Fut + Send + Sync + 'static,
    Fut: Future + Send,
    Fut::Output: Send,
{
    #[cfg(not(feature = "official"))]
    {
        use kovi::PluginBuilder;
        use kovi_onebot::EventRegistrar as _;
        PluginBuilder::on_msg(move |event| handler(event as Arc<dyn BotMsgEvent>));
    }
    #[cfg(feature = "official")]
    {
        use kovi::PluginBuilder;
        use kovi_official::event::EventRegistrar as _;
        PluginBuilder::on_msg(move |event| handler(event as Arc<dyn BotMsgEvent>));
    }
}

/// 只监听群消息事件。
pub fn on_group_msg<F, Fut>(handler: F)
where
    F: Fn(Arc<dyn BotMsgEvent>) -> Fut + Send + Sync + 'static,
    Fut: Future + Send,
    Fut::Output: Send,
{
    #[cfg(not(feature = "official"))]
    {
        use kovi::PluginBuilder;
        use kovi_onebot::EventRegistrar as _;
        PluginBuilder::on_group_msg(move |event| handler(event as Arc<dyn BotMsgEvent>));
    }
    #[cfg(feature = "official")]
    {
        use kovi::PluginBuilder;
        use kovi_official::event::EventRegistrar as _;
        PluginBuilder::on_group_msg(move |event| {
            handler(Arc::clone(&event.0) as Arc<dyn BotMsgEvent>)
        });
    }
}

/// 只监听私聊消息事件。
pub fn on_private_msg<F, Fut>(handler: F)
where
    F: Fn(Arc<dyn BotMsgEvent>) -> Fut + Send + Sync + 'static,
    Fut: Future + Send,
    Fut::Output: Send,
{
    #[cfg(not(feature = "official"))]
    {
        use kovi::PluginBuilder;
        use kovi_onebot::EventRegistrar as _;
        PluginBuilder::on_private_msg(move |event| handler(event as Arc<dyn BotMsgEvent>));
    }
    #[cfg(feature = "official")]
    {
        use kovi::PluginBuilder;
        use kovi_official::event::EventRegistrar as _;
        PluginBuilder::on_private_msg(move |event| {
            handler(Arc::clone(&event.0) as Arc<dyn BotMsgEvent>)
        });
    }
}

// ── 主动发送 ──────────────────────────────────────────────────────────────

/// 主动发群消息；`group` 为群身份键（官方栈 group_openid / OneBot 栈群号）。
pub async fn send_group_message(
    bot: &RuntimeBot,
    group: &str,
    message: Message,
) -> Result<(), String> {
    #[cfg(not(feature = "official"))]
    {
        use kovi_onebot::OnebotTrait as _;
        let group_id: i64 = group.parse().map_err(|_| format!("群号非法: {group}"))?;
        bot.send_group_msg_return(group_id, message)
            .await
            .map(|_| ())
            .map_err(|e| {
                e.message
                    .unwrap_or_else(|| format!("retcode {}", e.retcode))
            })
    }
    #[cfg(feature = "official")]
    {
        send_official(bot, "group", group, message, None).await
    }
}

/// 发单聊消息；`user` 为用户身份键。
pub async fn send_private_message(
    bot: &RuntimeBot,
    user: &str,
    message: Message,
) -> Result<(), String> {
    #[cfg(not(feature = "official"))]
    {
        use kovi_onebot::OnebotTrait as _;
        let user_id: i64 = user.parse().map_err(|_| format!("QQ 号非法: {user}"))?;
        bot.send_private_msg_return(user_id, message)
            .await
            .map(|_| ())
            .map_err(|e| {
                e.message
                    .unwrap_or_else(|| format!("retcode {}", e.retcode))
            })
    }
    #[cfg(feature = "official")]
    {
        send_official(bot, "private", user, message, None).await
    }
}

/// 记录 error 日志，并把同样的内容私聊给 bot 主管理员。
pub async fn notify_admin(bot: &RuntimeBot, message: String) {
    tracing::error!("{message}");
    let Some(admin_key) = bot.get_main_admin().ok().map(|admin| admin.to_string()) else {
        return;
    };
    if let Err(e) = send_private_message(bot, &admin_key, Message::from(message)).await {
        tracing::warn!("主管理员上报发送失败: {e}");
    }
}

#[cfg(feature = "official")]
async fn send_official(
    bot: &RuntimeBot,
    scope: &str,
    openid: &str,
    message: Message,
    reply_to: Option<&str>,
) -> Result<(), String> {
    use kovi::ApiReturn;
    use kovi::serde_json::json;

    let mut params = json!({
        "scope": scope,
        "openid": openid,
        "message": message_segments_json(&message),
    });
    if let Some(reply_to) = reply_to {
        params["reply_to"] = json!(reply_to);
    }
    match bot.send_api_return("official_send", params).await {
        Ok(_) => Ok(()),
        Err(ApiReturn {
            message: Some(message),
            ..
        }) => Err(message),
        Err(other) => Err(format!("retcode {}", other.retcode)),
    }
}

// ── 引用消息 ──────────────────────────────────────────────────────────────

/// 取 reply 段所引用消息里的图片段。官方栈的 reply 段由驱动按官方形状
/// 原样携带被引元素（图片在 attachments 里；卡片是 ark_data、没有图）；
/// OneBot 栈按段内 id 调 get_msg 回查。
pub async fn quoted_image_segments(
    bot: &RuntimeBot,
    reply: &kovi::message::Segment,
) -> Result<Vec<kovi::message::Segment>, String> {
    #[cfg(feature = "official")]
    {
        let _ = bot;
        let images = reply
            .data
            .get("attachments")
            .and_then(kovi::serde_json::Value::as_array)
            .map(|list| {
                list.iter()
                    .filter(|attachment| {
                        attachment
                            .get("content_type")
                            .and_then(kovi::serde_json::Value::as_str)
                            .is_some_and(|mime| mime.starts_with("image/"))
                    })
                    .filter_map(|attachment| {
                        attachment
                            .get("url")
                            .and_then(kovi::serde_json::Value::as_str)
                    })
                    .map(|url| kovi::message::Segment {
                        kind: "image".to_owned(),
                        data: kovi::serde_json::json!({ "file": url, "url": url }),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if images.is_empty() {
            return Err("引用消息里没有图片".to_owned());
        }
        Ok(images)
    }
    #[cfg(not(feature = "official"))]
    {
        use kovi_onebot::OnebotTrait as _;
        let id = reply
            .data
            .get("id")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
            })
            .ok_or_else(|| "引用消息缺少 id".to_owned())?;
        let id = i32::try_from(id).map_err(|_| "引用消息 id 非法".to_owned())?;
        let response = bot.get_msg(id).await.map_err(|e| {
            e.message
                .unwrap_or_else(|| format!("retcode {}", e.retcode))
        })?;
        if response.status != "ok" {
            return Err("无法读取引用的消息".to_owned());
        }
        let message = response
            .data
            .get("message")
            .cloned()
            .ok_or_else(|| "get_msg 返回缺少 message".to_owned())?;
        let segments: Vec<kovi::message::Segment> = kovi::serde_json::from_value(message)
            .map_err(|e| format!("解析引用消息段失败: {e}"))?;
        let images: Vec<_> = segments
            .into_iter()
            .filter(|segment| segment.kind == "image")
            .collect();
        if images.is_empty() {
            return Err("引用消息里没有图片".to_owned());
        }
        Ok(images)
    }
}

// ── 成员信息 ──────────────────────────────────────────────────────────────

/// 成员展示信息：昵称 / 群名片 / 头像。
#[derive(Debug, Clone)]
pub struct MemberProfile {
    pub nickname: String,
    pub card: Option<String>,
    pub avatar: Option<String>,
}

/// 查询群成员展示信息。官方栈来自事件流缓存（头像是官方下发的 URL）；
/// OneBot 栈查询协议端并用 QQ 号拼头像。
pub async fn member_profile(
    bot: &RuntimeBot,
    group: &str,
    user: &str,
) -> Result<MemberProfile, String> {
    #[cfg(not(feature = "official"))]
    {
        use kovi_onebot::OnebotTrait as _;
        let group_id: i64 = group.parse().map_err(|_| format!("群号非法: {group}"))?;
        let user_id: i64 = user.parse().map_err(|_| format!("QQ 号非法: {user}"))?;
        let response = bot
            .get_group_member_info(group_id, user_id, false)
            .await
            .map_err(|e| {
                e.message
                    .unwrap_or_else(|| format!("retcode {}", e.retcode))
            })?;
        let info = response.data;
        Ok(MemberProfile {
            nickname: info
                .get("nickname")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            card: info
                .get("card")
                .and_then(|v| v.as_str())
                .filter(|card| !card.is_empty())
                .map(str::to_owned),
            avatar: Some(format!("https://q4.qlogo.cn/headimg_dl?dst_uin={user_id}")),
        })
    }
    #[cfg(feature = "official")]
    {
        use kovi::serde_json::json;

        let params = json!({ "group_openid": group, "member_openid": user });
        let info = bot
            .send_api_return("official_member_info", params)
            .await
            .map_err(|e| {
                e.message
                    .unwrap_or_else(|| format!("retcode {}", e.retcode))
            })?;
        let pick = |key: &str| {
            info.data
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        };
        Ok(MemberProfile {
            nickname: pick("nickname").unwrap_or_default(),
            card: pick("card").filter(|card| !card.is_empty()),
            avatar: pick("avatar"),
        })
    }
}
