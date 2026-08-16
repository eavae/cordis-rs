//! Behavior-test support utilities (story card A2).
//!
//! Mirrors the helpers in `packages/core/tests/utils.ts` of the TypeScript
//! reference implementation. The fake clock is implemented manually (waker
//! table) instead of `tokio::time`, so that it works both for plain spawned
//! tasks and for `LocalSet`-scheduled fiber tasks.

use std::cell::RefCell;
use std::future::{Future, poll_fn};
use std::rc::Rc;
use std::task::{Poll, Waker};

pub mod fiber;
pub mod isolate;

/// A manually advanced fake clock (millisecond precision).
#[derive(Default)]
struct ClockState {
    now_ms: u64,
    next_id: u64,
    waiters: Vec<Waiter>,
}

struct Waiter {
    id: u64,
    deadline: u64,
    waker: Option<Waker>,
}

/// Handle to the fake clock created by [`with_timers`].
///
/// Equivalent to Vitest's fake timers (`vi.useFakeTimers`): while the handle
/// is live the clock is frozen and only moves when explicitly advanced.
#[derive(Clone)]
pub struct Timers {
    state: Rc<RefCell<ClockState>>,
}

impl Timers {
    /// Advances the fake clock by `ms` milliseconds, waking every timer that
    /// becomes due and yielding so woken tasks can resume.
    ///
    /// Equivalent to `vi.advanceTimersByTimeAsync(ms)`.
    pub async fn advance(&self, ms: u64) {
        let due_wakers = {
            let mut state = self.state.borrow_mut();
            state.now_ms += ms;
            let mut due = Vec::new();
            let mut index = 0;
            while index < state.waiters.len() {
                if state.waiters[index].deadline <= state.now_ms {
                    let mut waiter = state.waiters.remove(index);
                    if let Some(waker) = waiter.waker.take() {
                        due.push(waker);
                    }
                } else {
                    index += 1;
                }
            }
            due
        };
        for waker in due_wakers {
            waker.wake();
        }
        tokio::task::yield_now().await;
    }

    /// Sleeps for `ms` milliseconds of fake time.
    ///
    /// Equivalent to the `sleep()` helper in `utils.ts` (backed by
    /// `setTimeout`): the future only completes once the fake clock has been
    /// advanced far enough.
    pub fn sleep(&self, ms: u64) -> impl Future<Output = ()> {
        let state = self.state.clone();
        let deadline = state.borrow().now_ms + ms;
        let mut id: Option<u64> = None;
        poll_fn(move |cx| {
            let mut state = state.borrow_mut();
            if state.now_ms >= deadline {
                return Poll::Ready(());
            }
            match id {
                None => {
                    let new_id = state.next_id;
                    state.next_id += 1;
                    state.waiters.push(Waiter {
                        id: new_id,
                        deadline,
                        waker: Some(cx.waker().clone()),
                    });
                    id = Some(new_id);
                    Poll::Pending
                }
                Some(existing) => {
                    if let Some(waiter) = state.waiters.iter_mut().find(|w| w.id == existing) {
                        waiter.waker = Some(cx.waker().clone());
                    }
                    Poll::Pending
                }
            }
        })
    }

    /// The current fake-clock value in milliseconds since the clock started.
    pub fn now(&self) -> u64 {
        self.state.borrow().now_ms
    }
}

/// Run `body` with a fresh manual fake clock, mirroring `withTimers` in
/// `packages/core/tests/utils.ts`.
pub async fn with_timers<F, Fut>(body: F) -> Fut::Output
where
    F: FnOnce(Timers) -> Fut,
    Fut: Future,
{
    body(Timers {
        state: Rc::new(RefCell::new(ClockState::default())),
    })
    .await
}
