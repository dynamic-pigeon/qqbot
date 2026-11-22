//! 被动回复路由。
//!
//! 官方模型：携带 msg_id 的发送是「被动回复」，每条用户消息最多回 5 次，
//! 窗口群 5 分钟、单聊 60 分钟（send.md）；不带 msg_id 的是「主动消息」，
//! 需平台单独授权（实测默认无权限，HTTP 400 err_code 40034105）。kovi 侧
//! 的 `reply()` 在 action 层没有携带原消息 id，这里按会话记录最近收到的
//! 用户消息，发送时挑还有余量的那条作被动回复——没有引用目标的发送
//! （命令回执走 send_group_message、定时推送）同样从这里取槽位。

use std::collections::HashMap;
use std::time::{Duration, Instant};

const GROUP_PASSIVE_WINDOW: Duration = Duration::from_secs(5 * 60);
const USER_PASSIVE_WINDOW: Duration = Duration::from_secs(60 * 60);
const MAX_REPLIES_PER_MESSAGE: u32 = 5;

/// 会话键首段即会话类型（`g:` 群 / `u:` 单聊），窗口随之取值。
fn passive_window(conversation: &str) -> Duration {
    if conversation.starts_with("g:") {
        GROUP_PASSIVE_WINDOW
    } else {
        USER_PASSIVE_WINDOW
    }
}

pub struct RecentInbound {
    inner: std::sync::Mutex<HashMap<String, Vec<InboundMessage>>>,
}

struct InboundMessage {
    official_id: String,
    received_at: Instant,
    replies: u32,
}

/// 会话键：群为 `g:{group_openid}`，单聊为 `u:{user_openid}`。
pub fn conversation_key(kind: Conversation, openid: &str) -> String {
    match kind {
        Conversation::Group => format!("g:{openid}"),
        Conversation::User => format!("u:{openid}"),
    }
}

pub enum Conversation {
    Group,
    User,
}

impl RecentInbound {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, conversation: &str, official_message_id: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(conversation.to_owned())
            .or_default()
            .push(InboundMessage {
                official_id: official_message_id.to_owned(),
                received_at: Instant::now(),
                replies: 0,
            });
    }

    /// 取一个可用的被动回复槽位，返回 (官方消息 id, msg_seq)；
    /// 没有可用槽位（无近期消息或全部用满）时返回 None，走主动消息。
    pub fn take_reply_slot(&self, conversation: &str) -> Option<(String, u32)> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let queue = map.get_mut(conversation)?;
        let now = Instant::now();
        queue.retain(|message| {
            now.duration_since(message.received_at) < passive_window(conversation)
        });
        // 从最新往回找第一条还有回复余量的消息。
        let chosen = queue
            .iter_mut()
            .rev()
            .find(|message| message.replies < MAX_REPLIES_PER_MESSAGE)?;
        chosen.replies += 1;
        Some((chosen.official_id.clone(), chosen.replies))
    }

    /// 引用回复指定某条消息时，优先消耗那条消息的槽位。
    pub fn take_reply_slot_for(
        &self,
        conversation: &str,
        official_message_id: &str,
    ) -> Option<(String, u32)> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let queue = map.get_mut(conversation)?;
        let now = Instant::now();
        queue.retain(|message| {
            now.duration_since(message.received_at) < passive_window(conversation)
        });
        let chosen = queue.iter_mut().rev().find(|message| {
            message.official_id == official_message_id && message.replies < MAX_REPLIES_PER_MESSAGE
        })?;
        chosen.replies += 1;
        Some((chosen.official_id.clone(), chosen.replies))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 每条消息最多五个槽位() {
        let inbound = RecentInbound::new();
        inbound.register("g:x", "msg-1");
        for seq in 1..=5 {
            let (id, got_seq) = inbound.take_reply_slot("g:x").unwrap();
            assert_eq!(id, "msg-1");
            assert_eq!(got_seq, seq);
        }
        assert!(inbound.take_reply_slot("g:x").is_none());
    }

    #[test]
    fn 优先用最新的消息并顺延() {
        let inbound = RecentInbound::new();
        inbound.register("g:x", "msg-old");
        inbound.register("g:x", "msg-new");
        assert_eq!(inbound.take_reply_slot("g:x").unwrap().0, "msg-new");
        assert_eq!(inbound.take_reply_slot("g:x").unwrap().0, "msg-new");
        // 用满后回退到较早的消息。
        for _ in 3..=5 {
            inbound.take_reply_slot("g:x").unwrap();
        }
        assert_eq!(inbound.take_reply_slot("g:x").unwrap().0, "msg-old");
    }

    #[test]
    fn 无近期消息时无槽位() {
        let inbound = RecentInbound::new();
        assert!(inbound.take_reply_slot("g:none").is_none());
    }
}
