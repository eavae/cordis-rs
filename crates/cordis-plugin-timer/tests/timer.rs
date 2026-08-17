//! Timer plugin.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use cordis_core::{Context, Effect, Plugin};
use cordis_plugin_timer::TimerService;

const TICK: u64 = 40;

// All tests run on a paused clock (`start_paused`): time only moves through
// explicit advance calls, so the assertions are deterministic and independent
// of machine load.

/// Advances the paused clock, then yields so timer-woken tasks can run their
/// callbacks before the next assertion.
async fn advance_and_run(duration: Duration) {
    tokio::time::advance(duration).await;
    tokio::task::yield_now().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_basic_and_once() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let count = Rc::new(Cell::new(0u32));
            TimerService::timeout(
                &root,
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK,
            )
            .unwrap();
            // Let the spawned task register its timer at the current (paused)
            // clock before the first advance.
            tokio::task::yield_now().await;
            advance_and_run(Duration::from_millis(TICK / 2)).await;
            assert_eq!(count.get(), 0);
            advance_and_run(Duration::from_millis(TICK)).await;
            assert_eq!(count.get(), 1);
            advance_and_run(Duration::from_millis(TICK)).await;
            assert_eq!(count.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_dispose_cancels() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let count = Rc::new(Cell::new(0u32));
            let handle = TimerService::timeout(
                &root,
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK,
            )
            .unwrap();
            tokio::task::yield_now().await;
            handle.dispose().await.unwrap();
            advance_and_run(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_future_resolves() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let future = TimerService::timeout_future(&root, TICK);
            // Poll the future from a task so its timer registers with the
            // paused clock before the time advances.
            let handle = tokio::task::spawn_local(future);
            advance_and_run(Duration::from_millis(TICK)).await;
            handle.await.unwrap().unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn interval_repeats_and_stops() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let count = Rc::new(Cell::new(0u32));
            let handle = TimerService::interval(
                &root,
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK,
            )
            .unwrap();
            tokio::task::yield_now().await;
            advance_and_run(Duration::from_millis(TICK)).await;
            advance_and_run(Duration::from_millis(TICK)).await;
            assert_eq!(
                count.get(),
                2,
                "interval should tick twice after 2 intervals"
            );
            handle.dispose().await.unwrap();
            advance_and_run(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 2);
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn interval_ticks_collects() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let ticks = tokio::task::spawn_local(TimerService::interval_ticks(&root, TICK, 3));
            advance_and_run(Duration::from_millis(TICK * 3)).await;
            let ticks = ticks.await.unwrap();
            assert_eq!(ticks.len(), 3);
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn throttle_first_immediate_then_delayed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
            let throttled = TimerService::throttle(
                &root,
                {
                    let calls = calls.clone();
                    Rc::new(move || calls.borrow_mut().push(tokio::time::Instant::now()))
                },
                TICK * 2,
                false,
            )
            .unwrap();
            let start = tokio::time::Instant::now();
            throttled();
            tokio::task::yield_now().await;
            assert_eq!(calls.borrow().len(), 1, "first call is immediate");
            throttled();
            throttled();
            tokio::task::yield_now().await;
            assert_eq!(
                calls.borrow().len(),
                1,
                "calls within the window are delayed"
            );
            advance_and_run(Duration::from_millis(TICK * 3)).await;
            assert_eq!(calls.borrow().len(), 2, "one trailing call runs");
            assert!(calls.borrow()[1].duration_since(start) >= Duration::from_millis(TICK * 2));
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn throttle_no_trailing_drops() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let count = Rc::new(Cell::new(0u32));
            let throttled = TimerService::throttle(
                &root,
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK * 2,
                true,
            )
            .unwrap();
            throttled();
            throttled();
            throttled();
            advance_and_run(Duration::from_millis(TICK * 3)).await;
            assert_eq!(count.get(), 1, "no trailing call with no_trailing");
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn debounce_resets_and_fires_after_quiet() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let count = Rc::new(Cell::new(0u32));
            let debounced = TimerService::debounce(
                &root,
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK,
            )
            .unwrap();
            debounced();
            tokio::task::yield_now().await;
            advance_and_run(Duration::from_millis(TICK / 2)).await;
            debounced();
            tokio::task::yield_now().await;
            advance_and_run(Duration::from_millis(TICK / 2)).await;
            debounced();
            tokio::task::yield_now().await;
            assert_eq!(count.get(), 0, "pending during rapid calls");
            advance_and_run(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 1, "one call after the quiet period");
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn throttle_disposed_skips_trailing() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|_ctx: &Context, _config| Effect::None),
                },
                None,
            );
            fiber.wait().await.unwrap();
            let count = Rc::new(Cell::new(0u32));
            let throttled = TimerService::throttle(
                &fiber.context(),
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK,
                false,
            )
            .unwrap();

            fiber.dispose().await;
            // Immediate calls still fire after dispose; trailing ones do not.
            throttled();
            assert_eq!(count.get(), 1, "immediate call fires after dispose");
            throttled();
            advance_and_run(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 1, "no trailing call after dispose");
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn debounce_disposed_ignores_calls() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|_ctx: &Context, _config| Effect::None),
                },
                None,
            );
            fiber.wait().await.unwrap();
            let count = Rc::new(Cell::new(0u32));
            let debounced = TimerService::debounce(
                &fiber.context(),
                {
                    let count = count.clone();
                    Rc::new(move || count.set(count.get() + 1))
                },
                TICK,
            )
            .unwrap();

            debounced();
            fiber.dispose().await;
            debounced();
            advance_and_run(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 0, "no call after dispose");
        })
        .await;
}
