//! Cordis utils (Rust port).
//!
//! Port of `@cordisjs/utils`: the effect-bound ordered [`List`] collection.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use cordis_core::{Context, CordisError, Effect, EffectHandle, sync_disposer};

/// The inner item store.
type ItemStore<T> = Rc<RefCell<Vec<(u64, Rc<T>)>>>;

/// An effect-bound ordered collection (mirrors `utils.List`).
///
/// Items pushed with [`List::push`] are removed when the context's fiber
/// unloads (the push is an effect).
pub struct List<T> {
    sn: Cell<u64>,
    inner: ItemStore<T>,
}

impl<T: 'static> List<T> {
    /// Creates an empty list.
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    /// The number of items.
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends an item; it is removed when `ctx`'s fiber unloads.
    pub fn push(&self, ctx: &Context, value: T) -> Result<Rc<EffectHandle>, CordisError> {
        let sn = {
            let next = self.sn.get() + 1;
            self.sn.set(next);
            next
        };
        let inner = Rc::clone(&self.inner);
        ctx.fiber().effect(
            move || {
                inner.borrow_mut().push((sn, Rc::new(value)));
                Effect::Disposer(sync_disposer(move || {
                    inner.borrow_mut().retain(|(item_sn, _)| *item_sn != sn);
                }))
            },
            "list.push()",
        )
    }

    /// A snapshot iterator over the items.
    pub fn iter(&self) -> Vec<Rc<T>> {
        self.inner
            .borrow()
            .iter()
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// Filters the items.
    pub fn filter(&self, predicate: impl Fn(&T) -> bool) -> Vec<Rc<T>> {
        self.inner
            .borrow()
            .iter()
            .filter(|(_, value)| predicate(value))
            .map(|(_, value)| value.clone())
            .collect()
    }

    /// Maps the items.
    pub fn map<U>(&self, mapper: impl Fn(&T) -> U) -> Vec<U> {
        self.inner
            .borrow()
            .iter()
            .map(|(_, value)| mapper(value))
            .collect()
    }
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self {
            sn: Cell::new(0),
            inner: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

impl<T: fmt::Debug + 'static> fmt::Debug for List<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.iter().iter().map(|value| &**value))
            .finish()
    }
}
