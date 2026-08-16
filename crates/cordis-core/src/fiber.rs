//! Fiber lifecycle model.

use std::cell::Cell;

/// Lifecycle state of a [`Fiber`] (mirrors `FiberState` in the TS reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    /// The fiber is scheduled but not yet loading.
    Pending,
    /// The plugin entry is being applied.
    Loading,
    /// The plugin entry is active.
    Active,
    /// The plugin entry failed to apply.
    Failed,
    /// The fiber has been disposed.
    Disposed,
    /// The fiber is being unloaded.
    Unloading,
}

/// A plugin lifecycle unit.
///
/// Story card B1 only needs a minimal fiber for the root context; the full
/// state machine (inertia locks, config updates, restart) is implemented in
/// story card B2.
#[derive(Debug)]
pub struct Fiber {
    /// Monotonically increasing fiber id.
    pub uid: u64,
    /// Resolved fiber name (e.g. `root`).
    pub name: String,
    /// Current lifecycle state.
    pub state: Cell<FiberState>,
}

impl Fiber {
    /// Creates the root fiber of a context tree.
    pub(crate) fn root() -> Self {
        Fiber {
            uid: 0,
            name: "root".to_string(),
            state: Cell::new(FiberState::Active),
        }
    }
}
