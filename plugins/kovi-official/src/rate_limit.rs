//! 单会话发送节流。
//!
//! 官方频控里最紧的一条是单群每分钟最多 20 条，被动 / 主动消息都算。
//! 发送前在这里排队，窗口滑出后放行。

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub struct SendRateLimiter {
    window: Duration,
    max: usize,
    inner: std::sync::Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl SendRateLimiter {
    pub fn new(window: Duration, max: usize) -> Self {
        Self {
            window,
            max,
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 阻塞到该会话的发送窗口有空位。锁只覆盖队列操作，不跨 sleep。
    pub async fn acquire(&self, conversation: &str) {
        loop {
            let wait_until = {
                let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                let queue = map.entry(conversation.to_owned()).or_default();
                let now = Instant::now();
                while let Some(oldest) = queue.front() {
                    if now.duration_since(*oldest) >= self.window {
                        queue.pop_front();
                    } else {
                        break;
                    }
                }
                if queue.len() < self.max {
                    queue.push_back(now);
                    return;
                }
                // 窗口满：等到最旧一条滑出。
                Some(*queue.front().unwrap() + self.window)
            };

            if let Some(deadline) = wait_until {
                let wait = deadline.saturating_duration_since(Instant::now());
                kovi::tokio::time::sleep(wait).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 窗口内超出上限需要等待() {
        kovi::tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                let limiter = SendRateLimiter::new(Duration::from_millis(80), 2);
                limiter.acquire("g1").await;
                limiter.acquire("g1").await;
                // 第 3 条必须等第一条滑出窗口，不应该立刻返回。
                let start = Instant::now();
                limiter.acquire("g1").await;
                assert!(start.elapsed() >= Duration::from_millis(70));
            });
    }

    #[test]
    fn 不同会话互不影响() {
        kovi::tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                let limiter = SendRateLimiter::new(Duration::from_secs(60), 1);
                limiter.acquire("g1").await;
                let start = Instant::now();
                limiter.acquire("g2").await;
                assert!(start.elapsed() < Duration::from_millis(200));
            });
    }
}
