//! Integration tests for the behavior-test support helpers.

mod behavior;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[tokio::test]
async fn sleep_waits_for_clock_advance() {
    behavior::with_timers(|timers| async move {
        let fired = Arc::new(AtomicBool::new(false));
        let fired2 = Arc::clone(&fired);
        let timers2 = timers.clone();
        let handle = tokio::task::spawn(async move {
            timers2.sleep(100).await;
            fired2.store(true, Ordering::SeqCst);
        });
        // Let the task register its timer at t=0 (mirrors the TS test
        // where the timer starts as soon as it is created).
        tokio::task::yield_now().await;

        assert!(
            !fired.load(Ordering::SeqCst),
            "timer must not fire before deadline"
        );
        timers.advance(99).await;
        assert!(
            !fired.load(Ordering::SeqCst),
            "timer must not fire before deadline"
        );

        timers.advance(1).await;
        handle.await.expect("sleep task must complete");
        assert!(
            fired.load(Ordering::SeqCst),
            "timer must fire at the deadline"
        );
    })
    .await;
}

#[tokio::test]
async fn now_tracks_advanced_fake_time() {
    behavior::with_timers(|timers| async move {
        let start = timers.now();
        timers.advance(250).await;
        let mid = timers.now();
        assert_eq!(mid - start, 250);
        timers.advance(750).await;
        let end = timers.now();
        assert_eq!(end - start, 1000);
    })
    .await;
}
