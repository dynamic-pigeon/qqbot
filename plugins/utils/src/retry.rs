use std::time::Duration;

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

    #[tokio::test]
    async fn retry_async_with_backoff_retries_until_success() {
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
