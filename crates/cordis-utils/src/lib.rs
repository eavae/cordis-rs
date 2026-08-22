//! Cordis utils (Rust port).
//!
//! Port of `@cordisjs/utils`: the effect-bound ordered [`List`] collection.

use parking_lot::Mutex;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cordis_core::{Context, CordisError, Effect, EffectHandle, sync_disposer};

/// The inner item store.
type ItemStore<T> = Arc<Mutex<Vec<(u64, Arc<T>)>>>;

/// An effect-bound ordered collection (mirrors `utils.List`).
///
/// Items pushed with [`List::push`] are removed when the context's fiber
/// unloads (the push is an effect).
pub struct List<T> {
    sn: AtomicU64,
    inner: ItemStore<T>,
}

impl<T: Send + Sync + 'static> List<T> {
    /// Creates an empty list.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The number of items.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends an item; it is removed when `ctx`'s fiber unloads.
    pub fn push(&self, ctx: &Context, value: T) -> Result<Arc<EffectHandle>, CordisError> {
        let sn = { self.sn.fetch_add(1, Ordering::Relaxed) + 1 };
        let inner = Arc::clone(&self.inner);
        ctx.fiber().effect(
            move || {
                inner.lock().push((sn, Arc::new(value)));
                Effect::Disposer(sync_disposer(move || {
                    inner.lock().retain(|(item_sn, _)| *item_sn != sn);
                }))
            },
            "list.push()",
        )
    }

    /// A snapshot iterator over the items.
    pub fn iter(&self) -> Vec<Arc<T>> {
        self.inner
            .lock()
            .iter()
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// Filters the items.
    pub fn filter(&self, predicate: impl Fn(&T) -> bool) -> Vec<Arc<T>> {
        self.inner
            .lock()
            .iter()
            .filter(|(_, value)| predicate(value))
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// Maps the items.
    pub fn map<U>(&self, mapper: impl Fn(&T) -> U) -> Vec<U> {
        self.inner
            .lock()
            .iter()
            .map(|(_, value)| mapper(value))
            .collect()
    }
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self {
            sn: AtomicU64::new(0),
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<T: fmt::Debug + Send + Sync + 'static> fmt::Debug for List<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.iter().iter().map(|value| &**value))
            .finish()
    }
}
