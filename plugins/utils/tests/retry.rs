use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use utils::retry::{retry, retry_async, retry_async_with_backoff, retry_with_backoff};

#[test]
fn retry_returns_first_success_without_calling_again() {
    let calls = AtomicUsize::new(0);
    let result: Result<i32, &'static str> = retry(
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(42)
        },
        3,
    );
    assert_eq!(result, Ok(42));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn retry_eventually_succeeds_after_failures() {
    let calls = AtomicUsize::new(0);
    let result: Result<&'static str, &'static str> = retry(
        || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 { Err("transient") } else { Ok("ok") }
        },
        5,
    );
    assert_eq!(result, Ok("ok"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn retry_returns_last_error_after_exhausting_retries() {
    let calls = AtomicUsize::new(0);
    let result: Result<i32, &'static str> = retry(
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("boom")
        },
        2,
    );
    assert_eq!(result, Err("boom"));
    // retries=2 意味着总共最多调用 1 + 2 = 3 次（首次 + 2 次重试）。
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn retry_with_zero_retries_still_runs_once() {
    let calls = AtomicUsize::new(0);
    let result: Result<(), &'static str> = retry(
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("nope")
        },
        0,
    );
    assert_eq!(result, Err("nope"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_async_eventually_succeeds() {
    let calls = AtomicUsize::new(0);
    let result: Result<i32, &'static str> = retry_async(
        || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 1 { Err("once") } else { Ok(7) }
        },
        3,
    )
    .await;
    assert_eq!(result, Ok(7));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retry_async_returns_first_success_without_calling_again() {
    let calls = AtomicUsize::new(0);
    let result: Result<i32, &'static str> = retry_async(
        || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(99)
        },
        3,
    )
    .await;
    assert_eq!(result, Ok(99));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_async_with_zero_retries_still_runs_once() {
    let calls = AtomicUsize::new(0);
    let result: Result<(), &'static str> = retry_async(
        || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("nope")
        },
        0,
    )
    .await;
    assert_eq!(result, Err("nope"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_async_returns_error_after_exhausting_retries() {
    let calls = AtomicUsize::new(0);
    let result: Result<i32, &'static str> = retry_async(
        || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("permanent")
        },
        4,
    )
    .await;
    assert_eq!(result, Err("permanent"));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
}

#[test]
fn retry_with_backoff_waits_between_attempts() {
    let calls = AtomicUsize::new(0);
    let start = Instant::now();
    let result: Result<i32, &'static str> = retry_with_backoff(
        || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("fail")
        },
        2,
        Duration::from_millis(10),
        Duration::from_millis(50),
    );
    let elapsed = start.elapsed();
    assert_eq!(result, Err("fail"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    // 至少等待 base + base*2 = 30ms
    assert!(elapsed >= Duration::from_millis(30));
}

#[tokio::test]
async fn retry_async_with_backoff_eventually_succeeds() {
    let calls = AtomicUsize::new(0);
    let result: Result<i32, &'static str> = retry_async_with_backoff(
        || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 { Err("transient") } else { Ok(42) }
        },
        3,
        Duration::from_millis(1),
        Duration::from_millis(10),
    )
    .await;
    assert_eq!(result, Ok(42));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}
