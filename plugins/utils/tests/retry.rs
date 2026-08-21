use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use utils::retry::{retry, retry_async, retry_with_backoff};

#[test]
fn retry_counts_initial_attempt_plus_retries() {
    let ok = AtomicUsize::new(0);
    assert_eq!(
        retry(
            || {
                ok.fetch_add(1, Ordering::SeqCst);
                Ok::<_, &str>(1)
            },
            3
        ),
        Ok(1)
    );
    assert_eq!(ok.load(Ordering::SeqCst), 1);

    let mixed = AtomicUsize::new(0);
    assert_eq!(
        retry(
            || {
                let n = mixed.fetch_add(1, Ordering::SeqCst);
                if n < 2 { Err("transient") } else { Ok("ok") }
            },
            5
        ),
        Ok("ok")
    );
    assert_eq!(mixed.load(Ordering::SeqCst), 3);

    let fail = AtomicUsize::new(0);
    assert_eq!(
        retry(
            || {
                fail.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>("boom")
            },
            2
        ),
        Err("boom")
    );
    assert_eq!(fail.load(Ordering::SeqCst), 3);

    let once = AtomicUsize::new(0);
    assert_eq!(
        retry(
            || {
                once.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>("nope")
            },
            0
        ),
        Err("nope")
    );
    assert_eq!(once.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_async_counts_initial_attempt_plus_retries() {
    let mixed = AtomicUsize::new(0);
    assert_eq!(
        retry_async(
            || async {
                let n = mixed.fetch_add(1, Ordering::SeqCst);
                if n < 1 { Err("once") } else { Ok(7) }
            },
            3
        )
        .await,
        Ok(7)
    );
    assert_eq!(mixed.load(Ordering::SeqCst), 2);

    let fail = AtomicUsize::new(0);
    assert_eq!(
        retry_async(
            || async {
                fail.fetch_add(1, Ordering::SeqCst);
                Err::<i32, _>("permanent")
            },
            4
        )
        .await,
        Err("permanent")
    );
    assert_eq!(fail.load(Ordering::SeqCst), 5);

    let once = AtomicUsize::new(0);
    assert_eq!(
        retry_async(
            || async {
                once.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>("nope")
            },
            0
        )
        .await,
        Err("nope")
    );
    assert_eq!(once.load(Ordering::SeqCst), 1);
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
    assert_eq!(result, Err("fail"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(start.elapsed() >= Duration::from_millis(30));
}
