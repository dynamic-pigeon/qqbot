//! 被动回复槽位：每条入站消息可回复 5 次，群 5 分钟内、单聊 60 分钟内有效。

use std::collections::HashMap;
use std::time::Instant;

use crate::ChatScope;

const MAX_REPLIES_PER_MESSAGE: u32 = 5;

/// 被动回复的目标消息与序号。
pub type ReplySlot = (String, u32);

pub struct RecentInbound {
    inner: std::sync::Mutex<HashMap<String, Vec<InboundMessage>>>,
}

struct InboundMessage {
    official_id: String,
    received_at: Instant,
    replies: u32,
}

impl RecentInbound {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, scope: &ChatScope, official_message_id: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(scope.key()).or_default().push(InboundMessage {
            official_id: official_message_id.to_owned(),
            received_at: Instant::now(),
            replies: 0,
        });
    }

    /// 取一个可用的被动回复槽位；没有可用槽位（无近期消息或全部用满）时
    /// 返回 None，走主动消息。
    pub fn take_reply_slot(&self, scope: &ChatScope) -> Option<ReplySlot> {
        self.take(scope, None)
    }

    /// 引用回复指定某条消息时，优先消耗那条消息的槽位。
    pub fn take_reply_slot_for(
        &self,
        scope: &ChatScope,
        official_message_id: &str,
    ) -> Option<ReplySlot> {
        self.take(scope, Some(official_message_id))
    }

    /// `target` 为空时不限消息，否则只在该条消息里找。
    fn take(&self, scope: &ChatScope, target: Option<&str>) -> Option<ReplySlot> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let queue = map.get_mut(&scope.key())?;
        let now = Instant::now();
        queue.retain(|message| now.duration_since(message.received_at) < scope.passive_window());

        // 从最新往回找第一条还有回复余量的消息。
        let chosen = queue.iter_mut().rev().find(|message| {
            message.replies < MAX_REPLIES_PER_MESSAGE
                && target.is_none_or(|id| message.official_id == id)
        })?;
        chosen.replies += 1;
        Some((chosen.official_id.clone(), chosen.replies))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(openid: &str) -> ChatScope {
        ChatScope::Group(openid.to_owned())
    }

    #[test]
    fn 最新的消息优先每条最多五个槽位() {
        let inbound = RecentInbound::new();
        assert!(inbound.take_reply_slot(&group("x")).is_none());

        inbound.register(&group("x"), "msg-old");
        inbound.register(&group("x"), "msg-new");
        for seq in 1..=5 {
            let expected = Some(("msg-new".to_owned(), seq));
            assert_eq!(inbound.take_reply_slot(&group("x")), expected);
        }
        // 最新的一条用满后回退到较早的消息。
        for seq in 1..=5 {
            let expected = Some(("msg-old".to_owned(), seq));
            assert_eq!(inbound.take_reply_slot(&group("x")), expected);
        }
        assert!(inbound.take_reply_slot(&group("x")).is_none());
    }

    #[test]
    fn 群窗口比单聊短() {
        let stale = || InboundMessage {
            official_id: "msg-1".to_owned(),
            received_at: Instant::now() - std::time::Duration::from_secs(10 * 60),
            replies: 0,
        };
        let inbound = RecentInbound::new();
        inbound.inner.lock().unwrap().extend([
            ("g:u1".to_owned(), vec![stale()]),
            ("u:u1".to_owned(), vec![stale()]),
        ]);

        assert!(inbound.take_reply_slot(&group("u1")).is_none());
        let user = ChatScope::User("u1".to_owned());
        assert_eq!(
            inbound.take_reply_slot(&user),
            Some(("msg-1".to_owned(), 1))
        );
    }
}
