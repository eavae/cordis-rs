//! Behavior-test support utilities (story card A2).
//!
//! Mirrors the helpers in `packages/core/tests/utils.ts` of the TypeScript
//! reference implementation, adapted to tokio's fake clock.

use std::future::Future;
use std::time::Duration;

pub mod isolate;

/// Handle to the paused fake clock created by [`with_timers`].
///
/// Equivalent to Vitest's fake timers (`vi.useFakeTimers`): while the handle
/// is live the tokio clock is frozen and only moves when explicitly advanced.
#[derive(Clone, Debug)]
pub struct Timers;

impl Timers {
    /// Advance the fake clock by `ms` milliseconds, resolving every timer
    /// that becomes due.
    ///
    /// Equivalent to `vi.advanceTimersByTimeAsync(ms)`.
    pub async fn advance(&self, ms: u64) {
        tokio::time::advance(Duration::from_millis(ms)).await;
    }

    /// Sleep for `ms` milliseconds of fake time.
    ///
    /// Equivalent to the `sleep()` helper in `utils.ts` (which is backed by
    /// `setTimeout`): the future only completes once the fake clock has been
    /// advanced far enough.
    pub async fn sleep(&self, ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    /// The current fake-clock instant.
    ///
    /// Frozen while the clock is paused; moves forward on [`advance`] and
    /// [`sleep`] only.
    pub fn now(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }
}

/// Run `body` with the tokio clock paused, mirroring `withTimers` in
/// `packages/core/tests/utils.ts`.
///
/// The clock is resumed even if `body` panics.
pub async fn with_timers<F, Fut>(body: F) -> Fut::Output
where
    F: FnOnce(Timers) -> Fut,
    Fut: Future,
{
    tokio::time::pause();
    let _guard = ResumeGuard;
    body(Timers).await
}

/// Resumes the paused tokio clock when dropped.
struct ResumeGuard;

impl Drop for ResumeGuard {
    fn drop(&mut self) {
        tokio::time::resume();
    }
}
