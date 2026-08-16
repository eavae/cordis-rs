//! Cordis timer plugin (Rust port).
//!
//! Port of `@cordisjs/plugin-timer`: `ctx.timeout`, `ctx.interval`,
//! `ctx.throttle` and `ctx.debounce`, all bound to the fiber lifecycle.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
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
        callback: Rc<dyn Fn()>,
        delay: u64,
    ) -> Result<Rc<EffectHandle>, CordisError> {
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
        callback: Rc<dyn Fn()>,
        delay: u64,
    ) -> Result<Rc<EffectHandle>, CordisError> {
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
        callback: Rc<dyn Fn()>,
        delay: u64,
        no_trailing: bool,
    ) -> Result<Rc<dyn Fn()>, CordisError> {
        struct State {
            last: Option<tokio::time::Instant>,
            pending: bool,
        }
        let state = Rc::new(RefCell::new(State {
            last: None,
            pending: false,
        }));
        let handles = track_handles(ctx)?;
        let throttled: Rc<dyn Fn()> = Rc::new(move || {
            let now = tokio::time::Instant::now();
            let elapsed_ok = state
                .borrow()
                .last
                .map(|last| now.duration_since(last) >= Duration::from_millis(delay))
                .unwrap_or(true);
            if elapsed_ok {
                state.borrow_mut().last = Some(now);
                state.borrow_mut().pending = false;
                callback();
            } else if !no_trailing && !state.borrow().pending {
                state.borrow_mut().pending = true;
                let deadline: tokio::time::Instant =
                    state.borrow().last.unwrap() + Duration::from_millis(delay);
                let state_for_task = Rc::clone(&state);
                let callback = callback.clone();
                let state = state_for_task;
                let handle = tokio::task::spawn_local(async move {
                    tokio::time::sleep_until(deadline).await;
                    let mut state = state.borrow_mut();
                    state.last = Some(tokio::time::Instant::now());
                    state.pending = false;
                    drop(state);
                    callback();
                });
                handles.borrow_mut().push(handle);
            }
        });
        Ok(throttled)
    }

    /// Returns a debounced callback: consecutive calls reset the timer.
    pub fn debounce(
        ctx: &cordis_core::Context,
        callback: Rc<dyn Fn()>,
        delay: u64,
    ) -> Result<Rc<dyn Fn()>, CordisError> {
        let generation = Rc::new(Cell::new(0u64));
        let handles = track_handles(ctx)?;
        let debounced: Rc<dyn Fn()> = Rc::new(move || {
            let next_gen = generation.get() + 1;
            generation.set(next_gen);
            let callback = callback.clone();
            let generation = generation.clone();
            let handle = tokio::task::spawn_local(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if generation.get() == next_gen {
                    callback();
                }
            });
            handles.borrow_mut().push(handle);
        });
        Ok(debounced)
    }
}

fn track_handles(
    ctx: &cordis_core::Context,
) -> Result<Rc<RefCell<Vec<JoinHandle<()>>>>, CordisError> {
    let handles: Rc<RefCell<Vec<JoinHandle<()>>>> = Rc::new(RefCell::new(Vec::new()));
    let tracked = handles.clone();
    ctx.fiber().effect(
        move || {
            Effect::Disposer(sync_disposer(move || {
                for handle in tracked.borrow().iter() {
                    handle.abort();
                }
            }))
        },
        "ctx.timer-tracker()",
    )?;
    Ok(handles)
}
