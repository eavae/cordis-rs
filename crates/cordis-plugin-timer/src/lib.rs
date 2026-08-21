//! Cordis timer plugin (Rust port).
//!
//! Port of `@cordisjs/plugin-timer`: `ctx.timeout`, `ctx.interval`,
//! `ctx.throttle` and `ctx.debounce`, all bound to the fiber lifecycle.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis_core::{CordisError, Effect, EffectHandle, Service, sync_disposer};
use tokio::task::JoinHandle;

/// Timer service, available on every context as `ctx.timer`.
#[derive(Default)]
pub struct TimerService;

impl Service for TimerService {
    const NAME: &'static str = "timer";
}

impl TimerService {
    /// Runs `callback` once after `delay` milliseconds (dispose cancels).
    pub fn timeout(
        ctx: &cordis_core::Context,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay: u64,
    ) -> Result<Arc<EffectHandle>, CordisError> {
        ctx.fiber().effect(
            move || {
                let join = tokio::task::spawn_local(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    callback();
                });
                Effect::Disposer(Box::new(move || {
                    let join = join;
                    Box::pin(async move {
                        join.abort();
                        Ok(())
                    })
                }))
            },
            "ctx.timeout()",
        )
    }

    /// A future resolving after `delay` milliseconds (mirrors `timeout(ms)`).
    pub fn timeout_future(
        _ctx: &cordis_core::Context,
        delay: u64,
    ) -> cordis_core::BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok(())
        })
    }

    /// Runs `callback` every `delay` milliseconds (dispose cancels).
    pub fn interval(
        ctx: &cordis_core::Context,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay: u64,
    ) -> Result<Arc<EffectHandle>, CordisError> {
        ctx.fiber().effect(
            move || {
                let join = tokio::task::spawn_local(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        callback();
                    }
                });
                Effect::Disposer(Box::new(move || {
                    let join = join;
                    Box::pin(async move {
                        join.abort();
                        Ok(())
                    })
                }))
            },
            "ctx.interval()",
        )
    }

    /// Collects `count` interval ticks (mirrors consuming the async
    /// iterator a bounded number of times).
    pub fn interval_ticks(
        _ctx: &cordis_core::Context,
        delay: u64,
        count: usize,
    ) -> cordis_core::BoxFuture<'static, Vec<()>> {
        Box::pin(async move {
            let mut ticks = Vec::new();
            for _ in 0..count {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                ticks.push(());
            }
            ticks
        })
    }

    /// Returns a throttled callback: the first call runs immediately, calls
    /// within the window are delayed until the window ends (`no_trailing`
    /// drops them).
    pub fn throttle(
        ctx: &cordis_core::Context,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay: u64,
        no_trailing: bool,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, CordisError> {
        struct State {
            last: Option<tokio::time::Instant>,
            pending: bool,
        }
        let state = Arc::new(Mutex::new(State {
            last: None,
            pending: false,
        }));
        let tracker = track_handles(ctx)?;
        let throttled: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let now = tokio::time::Instant::now();
            let elapsed_ok = state
                .lock()
                .unwrap()
                .last
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(delay));
            if elapsed_ok {
                state.lock().unwrap().last = Some(now);
                state.lock().unwrap().pending = false;
                callback();
            } else if !tracker.disposed.load(Ordering::Acquire)
                && !no_trailing
                && !state.lock().unwrap().pending
            {
                let mut state_guard = state.lock().unwrap();
                state_guard.pending = true;
                let deadline: tokio::time::Instant =
                    state_guard.last.unwrap() + Duration::from_millis(delay);
                drop(state_guard);
                let callback = callback.clone();
                let state = state.clone();
                let disposed = tracker.disposed.clone();
                let handle = tokio::task::spawn_local(async move {
                    tokio::time::sleep_until(deadline).await;
                    if disposed.load(Ordering::Acquire) {
                        return;
                    }
                    let mut state = state.lock().unwrap();
                    state.last = Some(tokio::time::Instant::now());
                    state.pending = false;
                    drop(state);
                    callback();
                });
                tracker.handles.lock().unwrap().push(handle);
            }
        });
        Ok(throttled)
    }

    /// Returns a debounced callback: consecutive calls reset the timer.
    pub fn debounce(
        ctx: &cordis_core::Context,
        callback: Arc<dyn Fn() + Send + Sync>,
        delay: u64,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, CordisError> {
        let generation = Arc::new(AtomicU64::new(0));
        let tracker = track_handles(ctx)?;
        let debounced: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if tracker.disposed.load(Ordering::Acquire) {
                return;
            }
            let next_gen = generation.fetch_add(1, Ordering::AcqRel) + 1;
            let callback = callback.clone();
            let generation = generation.clone();
            let disposed = tracker.disposed.clone();
            let handle = tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if !disposed.load(Ordering::Acquire)
                    && generation.load(Ordering::Acquire) == next_gen
                {
                    callback();
                }
            });
            tracker.handles.lock().unwrap().push(handle);
        });
        Ok(debounced)
    }
}

/// Tracks spawned timer tasks and the fiber-disposed flag (mirrors the TS
/// `_schedule` disposer, which sets `isDisposed` and clears the timer).
struct TimerTracker {
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
    disposed: Arc<AtomicBool>,
}

fn track_handles(ctx: &cordis_core::Context) -> Result<TimerTracker, CordisError> {
    let handles: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
    let disposed = Arc::new(AtomicBool::new(false));
    let tracked_handles = handles.clone();
    let tracked_disposed = disposed.clone();
    ctx.fiber().effect(
        move || {
            Effect::Disposer(sync_disposer(move || {
                tracked_disposed.store(true, Ordering::Release);
                for handle in tracked_handles.lock().unwrap().iter() {
                    handle.abort();
                }
            }))
        },
        "ctx.timer-tracker()",
    )?;
    Ok(TimerTracker { handles, disposed })
}
