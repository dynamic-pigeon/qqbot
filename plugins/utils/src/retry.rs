use std::time::Duration;

pub fn retry<F, T, E>(mut f: F, retries: usize) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
{
    let mut attempts = 0;
    loop {
        match f() {
            Ok(result) => return Ok(result),
            Err(err) => {
                attempts += 1;
                if attempts > retries {
                    return Err(err);
                }
            }
        }
    }
}

pub async fn retry_async<F, Fut, T, E>(mut f: F, retries: usize) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>> + Send,
{
    let mut attempts = 0;
    loop {
        match f().await {
            Ok(result) => return Ok(result),
            Err(err) => {
                attempts += 1;
                if attempts > retries {
                    return Err(err);
                }
            }
        }
    }
}

/// 计算第 `attempt` 次重试前的退避延迟（从 0 开始计数）。
///
/// 延迟按 `base * 2^attempt` 指数增长，但不超过 `max`。
fn backoff_delay(attempt: usize, base: Duration, max: Duration) -> Duration {
    let multiplier = 2_u32.saturating_pow(attempt.min(31) as u32);
    base.saturating_mul(multiplier).min(max)
}

/// 带指数退避的同步重试。
///
/// 首次调用失败后，第 `n` 次重试前会等待 `min(base * 2^n, max)` 时长，
/// 以缓解服务端瞬时压力或网络抖动造成的连续失败。
///
/// 注意：本函数使用 `std::thread::sleep` 阻塞当前线程，不要在 async runtime
/// 的工作线程上直接调用，否则会阻塞 tokio executor。异步场景请使用
/// [`retry_async_with_backoff`]。
pub fn retry_with_backoff<F, T, E>(
    mut f: F,
    retries: usize,
    base_delay: Duration,
    max_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
{
    let mut attempts = 0;
    loop {
        match f() {
            Ok(result) => return Ok(result),
            Err(err) => {
                attempts += 1;
                if attempts > retries {
                    return Err(err);
                }
                std::thread::sleep(backoff_delay(attempts - 1, base_delay, max_delay));
            }
        }
    }
}

/// 带指数退避的异步重试。
pub async fn retry_async_with_backoff<F, Fut, T, E>(
    mut f: F,
    retries: usize,
    base_delay: Duration,
    max_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>> + Send,
{
    let mut attempts = 0;
    loop {
        match f().await {
            Ok(result) => return Ok(result),
            Err(err) => {
                attempts += 1;
                if attempts > retries {
                    return Err(err);
                }
                kovi::tokio::time::sleep(backoff_delay(attempts - 1, base_delay, max_delay)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn backoff_delay_grows_exponentially_then_caps() {
        let base = Duration::from_millis(10);
        let max = Duration::from_millis(100);
        assert_eq!(backoff_delay(0, base, max), Duration::from_millis(10));
        assert_eq!(backoff_delay(1, base, max), Duration::from_millis(20));
        assert_eq!(backoff_delay(2, base, max), Duration::from_millis(40));
        assert_eq!(backoff_delay(3, base, max), Duration::from_millis(80));
        assert_eq!(backoff_delay(4, base, max), Duration::from_millis(100));
        assert_eq!(backoff_delay(10, base, max), Duration::from_millis(100));
    }

    #[test]
    fn backoff_delay_clamps_edges() {
        let max = Duration::from_secs(1);
        assert_eq!(
            backoff_delay(31, Duration::from_millis(1), max),
            backoff_delay(100, Duration::from_millis(1), max)
        );
        assert_eq!(backoff_delay(5, Duration::ZERO, max), Duration::ZERO);
        assert_eq!(
            backoff_delay(5, Duration::from_secs(1), Duration::ZERO),
            Duration::ZERO
        );
        assert_eq!(backoff_delay(0, Duration::from_secs(10), max), max);
        assert_eq!(backoff_delay(10, Duration::from_secs(10), max), max);
    }

    #[tokio::test]
    async fn retry_async_with_backoff_retries_until_success() {
        // 前两次失败、第三次成功：总调用 3 次并返回成功值
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = retry_async_with_backoff(
            {
                let attempts = std::sync::Arc::clone(&attempts);
                move || {
                    let attempts = std::sync::Arc::clone(&attempts);
                    async move {
                        let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        if n < 3 { Err("boom") } else { Ok(42) }
                    }
                }
            },
            2,
            Duration::from_millis(1),
            Duration::from_millis(2),
        )
        .await;
        assert_eq!(result, Ok(42));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_async_with_backoff_gives_up_after_retries() {
        // 持续失败：retries=2 时总调用 3 次（1 次尝试 + 2 次重试）后返回错误
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = retry_async_with_backoff(
            {
                let attempts = std::sync::Arc::clone(&attempts);
                move || {
                    let attempts = std::sync::Arc::clone(&attempts);
                    async move {
                        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Err::<i32, &str>("boom")
                    }
                }
            },
            2,
            Duration::from_millis(1),
            Duration::from_millis(2),
        )
        .await;
        assert_eq!(result, Err("boom"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
