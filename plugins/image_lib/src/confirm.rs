use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// 「删除 <库名>」登记到「图库 确认」执行之间的等待窗口。
/// 文案里的分钟数按 as_secs() / 60 换算，改动时保持 60 的整数倍。
const PENDING_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ConfirmKey {
    pub group_id: i64,
    pub user_id: i64,
}

struct PendingWipe {
    library: String,
    created: Instant,
}

/// 待确认的清空操作。群 × 用户只保留最后一次登记，覆盖旧的待确认。
pub struct WipeConfirmations {
    inner: Mutex<HashMap<ConfirmKey, PendingWipe>>,
}

impl WipeConfirmations {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ConfirmKey, PendingWipe>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn begin(&self, key: ConfirmKey, library: String) {
        let mut inner = self.lock();
        expire(&mut inner);
        inner.insert(
            key,
            PendingWipe {
                library,
                created: Instant::now(),
            },
        );
    }

    /// 取出待确认清空的规范库名，取出即消费；确认与撤销共用。
    /// 没有登记或已超窗口返回 None。
    pub fn take(&self, key: &ConfirmKey) -> Option<String> {
        let mut inner = self.lock();
        expire(&mut inner);
        inner.remove(key).map(|pending| pending.library)
    }

    pub fn pending_ttl_minutes(&self) -> u64 {
        PENDING_TTL.as_secs() / 60
    }
}

fn expire(sessions: &mut HashMap<ConfirmKey, PendingWipe>) {
    sessions.retain(|_, pending| pending.created.elapsed() < PENDING_TTL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(group_id: i64, user_id: i64) -> ConfirmKey {
        ConfirmKey { group_id, user_id }
    }

    #[test]
    fn begin_then_take_consumes_once() {
        let confirmations = WipeConfirmations::new();
        let key = key(1, 2);
        confirmations.begin(key.clone(), "猫".to_owned());

        assert_eq!(confirmations.take(&key).as_deref(), Some("猫"));
        assert_eq!(confirmations.take(&key), None);
    }

    #[test]
    fn keys_isolate_by_group_and_user() {
        let confirmations = WipeConfirmations::new();
        confirmations.begin(key(1, 2), "猫".to_owned());
        confirmations.begin(key(1, 3), "狗".to_owned());
        confirmations.begin(key(2, 2), "鸟".to_owned());

        assert_eq!(confirmations.take(&key(1, 2)).as_deref(), Some("猫"));
        assert_eq!(confirmations.take(&key(1, 3)).as_deref(), Some("狗"));
        assert_eq!(confirmations.take(&key(2, 2)).as_deref(), Some("鸟"));
    }

    #[test]
    fn begin_overrides_previous_pending() {
        let confirmations = WipeConfirmations::new();
        let key = key(1, 2);
        confirmations.begin(key.clone(), "猫".to_owned());
        confirmations.begin(key.clone(), "狗".to_owned());

        assert_eq!(confirmations.take(&key).as_deref(), Some("狗"));
    }
}
