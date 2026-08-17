//! 按 key 的滑动窗口限流。
//!
//! 每次成功调用记一个时间点；窗口内达到上限后拒绝，并给出最早那次记录
//! 滑出窗口还需等待的时间。顺带清掉过期键，避免 map 随见过的 key 无限增长。

use std::{
    collections::HashMap,
    hash::Hash,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitHit {
    pub retry_after: Duration,
}

impl RateLimitHit {
    pub fn retry_after_secs(&self) -> u64 {
        self.retry_after.as_secs().max(1)
    }
}

pub struct RateLimiter<K> {
    window: Duration,
    max_per_window: usize,
    entries: Mutex<HashMap<K, Vec<Instant>>>,
}

impl<K: Eq + Hash> RateLimiter<K> {
    pub fn new(window: Duration, max_per_window: usize) -> Self {
        Self {
            window,
            max_per_window: max_per_window.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 只检查、不记账。用来在做重活之前先拒绝已经打满的 key。
    pub fn check(&self, key: &K) -> Result<(), RateLimitHit> {
        let mut map = self.lock();
        let now = Instant::now();
        prune(&mut map, now, self.window);
        if let Some(times) = map.get(key)
            && times.len() >= self.max_per_window
        {
            return Err(hit(times, now, self.window));
        }
        Ok(())
    }

    /// 窗口内还有名额则记一次并放行；否则返回还要等多久。
    pub fn try_acquire(&self, key: K) -> Result<(), RateLimitHit> {
        let mut map = self.lock();
        let now = Instant::now();
        prune(&mut map, now, self.window);
        let times = map.entry(key).or_default();
        if times.len() >= self.max_per_window {
            return Err(hit(times, now, self.window));
        }
        times.push(now);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, Vec<Instant>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn prune<K: Eq + Hash>(map: &mut HashMap<K, Vec<Instant>>, now: Instant, window: Duration) {
    map.retain(|_, times| {
        times.retain(|t| now.duration_since(*t) < window);
        !times.is_empty()
    });
}

fn hit(times: &[Instant], now: Instant, window: Duration) -> RateLimitHit {
    let oldest = times.first().copied().unwrap_or(now);
    let elapsed = now.saturating_duration_since(oldest);
    RateLimitHit {
        retry_after: window.saturating_sub(elapsed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks_per_key() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 3);
        assert!(limiter.try_acquire(1).is_ok());
        assert!(limiter.try_acquire(1).is_ok());
        assert!(limiter.try_acquire(1).is_ok());
        let hit = limiter.try_acquire(1).unwrap_err();
        assert!(hit.retry_after <= Duration::from_secs(60));
        assert!(hit.retry_after_secs() >= 1);
        assert!(limiter.try_acquire(2).is_ok());
    }

    #[test]
    fn window_expiry_frees_a_slot() {
        let limiter = RateLimiter::new(Duration::from_millis(40), 1);
        assert!(limiter.try_acquire("a").is_ok());
        assert!(limiter.try_acquire("a").is_err());
        std::thread::sleep(Duration::from_millis(50));
        assert!(limiter.try_acquire("a").is_ok());
    }

    #[test]
    fn zero_max_is_treated_as_one() {
        let limiter = RateLimiter::<u8>::new(Duration::from_secs(1), 0);
        assert!(limiter.try_acquire(1).is_ok());
        assert!(limiter.try_acquire(1).is_err());
    }

    #[test]
    fn check_does_not_consume_a_slot() {
        let limiter = RateLimiter::new(Duration::from_secs(60), 1);
        assert!(limiter.check(&1).is_ok());
        assert!(limiter.check(&1).is_ok());
        assert!(limiter.try_acquire(1).is_ok());
        assert!(limiter.check(&1).is_err());
    }
}
