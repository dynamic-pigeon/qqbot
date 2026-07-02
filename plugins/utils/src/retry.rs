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
    fn backoff_delay_clamps_attempt_and_saturates() {
        // attempt 超过 31 后仍按 attempt=31 计算（不 panic、不溢出）
        let base = Duration::from_millis(1);
        let max = Duration::from_secs(1);
        let at_31 = backoff_delay(31, base, max);
        let at_100 = backoff_delay(100, base, max);
        assert_eq!(at_31, at_100);
        assert_eq!(at_31, max);
    }

    #[test]
    fn backoff_delay_zero_base() {
        assert_eq!(
            backoff_delay(5, Duration::ZERO, Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn backoff_delay_zero_max() {
        // max 为 0 时结果恒为 0
        assert_eq!(
            backoff_delay(5, Duration::from_secs(1), Duration::ZERO),
            Duration::ZERO
        );
    }

    #[test]
    fn backoff_delay_base_greater_than_max() {
        // base > max 时取 min，结果恒为 max
        assert_eq!(
            backoff_delay(0, Duration::from_secs(10), Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            backoff_delay(10, Duration::from_secs(10), Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }
}
