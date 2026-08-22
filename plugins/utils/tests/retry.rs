use std::sync::atomic::{AtomicUsize, Ordering};
use utils::retry::retry_async;

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
