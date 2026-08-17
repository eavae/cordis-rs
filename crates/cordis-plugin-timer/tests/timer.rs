//! Story card D2: timer 插件.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use cordis_core::{Context, Effect, Plugin};
use cordis_plugin_timer::TimerService;

const TICK: u64 = 40;

#[tokio::test(flavor = "current_thread")]
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
            tokio::time::sleep(Duration::from_millis(TICK / 2)).await;
            assert_eq!(count.get(), 0);
            tokio::time::sleep(Duration::from_millis(TICK)).await;
            assert_eq!(count.get(), 1);
            tokio::time::sleep(Duration::from_millis(TICK)).await;
            assert_eq!(count.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            handle.dispose().await.unwrap();
            tokio::time::sleep(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn timeout_future_resolves() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let future = TimerService::timeout_future(&root, TICK);
            tokio::time::sleep(Duration::from_millis(TICK / 2)).await;
            future.await.unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            tokio::time::sleep(Duration::from_millis(TICK * 3)).await;
            assert!(
                count.get() >= 2,
                "interval should tick at least twice after 3 intervals"
            );
            handle.dispose().await.unwrap();
            tokio::time::sleep(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 2);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn interval_ticks_collects() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let ticks = TimerService::interval_ticks(&root, TICK, 3).await;
            assert_eq!(ticks.len(), 3);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            assert_eq!(calls.borrow().len(), 1, "first call is immediate");
            throttled();
            throttled();
            assert_eq!(
                calls.borrow().len(),
                1,
                "calls within the window are delayed"
            );
            tokio::time::sleep(Duration::from_millis(TICK * 3)).await;
            assert_eq!(calls.borrow().len(), 2, "one trailing call runs");
            assert!(calls.borrow()[1].duration_since(start) >= Duration::from_millis(TICK * 2));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            tokio::time::sleep(Duration::from_millis(TICK * 3)).await;
            assert_eq!(count.get(), 1, "no trailing call with no_trailing");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            tokio::time::sleep(Duration::from_millis(TICK / 2)).await;
            debounced();
            tokio::time::sleep(Duration::from_millis(TICK / 2)).await;
            debounced();
            assert_eq!(count.get(), 0, "pending during rapid calls");
            tokio::time::sleep(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 1, "one call after the quiet period");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            tokio::time::sleep(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 1, "no trailing call after dispose");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
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
            tokio::time::sleep(Duration::from_millis(TICK * 2)).await;
            assert_eq!(count.get(), 0, "no call after dispose");
        })
        .await;
}
