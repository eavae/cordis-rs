//! Context: the object every plugin receives.
//!
//! A [`Context`] owns a shareable [`Fiber`], a shared service store and two
//! immutable chain layers:
//!
//! - the *isolate* chain maps service names to labels. Services provided in
//!   an isolated context are only visible to contexts that share the label;
//! - the *intercept* chain collects per-service config overrides that are
//!   merged by [`Context::resolve_config`].

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::fiber::{Fiber, FiberState};
use crate::service::{Config, Disposer, Service};
use crate::{EventsService, LoggerService, ReflectService, RegistryService};

static NEXT_LABEL_ID: AtomicU64 = AtomicU64::new(1);

/// A service label. Labels compare by value: contexts isolated with the same
/// label share the same service instance (mirrors `Symbol('name')` equality
/// in the TS reference).
pub type Label = Rc<str>;

/// One immutable layer of the isolate chain.
#[derive(Debug, Default)]
pub(crate) struct IsolateLayer {
    entries: RefCell<HashMap<String, Label>>,
    parent: Option<Rc<IsolateLayer>>,
}

impl IsolateLayer {
    fn lookup(&self, name: &str) -> Option<Label> {
        if let Some(label) = self.entries.borrow().get(name) {
            return Some(label.clone());
        }
        self.parent.as_ref()?.lookup(name)
    }

    /// Returns the bottom-most (root) layer of the chain.
    fn bottom(&self) -> &IsolateLayer {
        let mut layer = self;
        while let Some(parent) = &layer.parent {
            layer = parent;
        }
        layer
    }

    fn insert(&self, name: &str, label: Label) {
        self.entries.borrow_mut().insert(name.to_string(), label);
    }
}

/// One immutable layer of the intercept chain.
#[derive(Debug, Default)]
pub(crate) struct InterceptLayer {
    entries: RefCell<HashMap<String, Rc<dyn Any>>>,
    parent: Option<Rc<InterceptLayer>>,
}

/// An entry of the shared service store.
#[derive(Debug)]
pub(crate) struct StoreEntry {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub fiber: Rc<Fiber>,
}

/// The service store shared by a whole context chain.
#[derive(Debug, Default)]
pub(crate) struct Store {
    by_label: HashMap<Label, StoreEntry>,
}

/// Shared inner state of a [`Context`].
#[derive(Debug)]
pub(crate) struct ContextInner {
    pub isolate: Rc<IsolateLayer>,
    pub intercept: Rc<InterceptLayer>,
    pub store: Rc<RefCell<Store>>,
    pub fiber: Rc<Fiber>,
    pub meta: RefCell<Vec<(String, Rc<dyn Any>)>>,
}

/// The core object handed to plugins.
///
/// A context is intentionally `!Send`: the whole runtime is single-threaded
/// and uses `Rc`/`RefCell` without locks (difficulty 3 decision).
#[derive(Clone, Debug)]
pub struct Context {
    pub(crate) inner: Rc<ContextInner>,
}

impl Context {
    /// Creates a new root context.
    ///
    /// The root owns an `ACTIVE` fiber and provides the four framework
    /// services (`events`, `logger`, `reflect`, `registry`).
    pub fn new() -> Context {
        let isolate = Rc::new(IsolateLayer::default());
        let intercept = Rc::new(InterceptLayer::default());
        let store = Rc::new(RefCell::new(Store::default()));
        let fiber = Rc::new(Fiber::root());
        let ctx = Context {
            inner: Rc::new(ContextInner {
                isolate,
                intercept,
                store,
                fiber,
                meta: RefCell::new(Vec::new()),
            }),
        };

        // Framework services are visible on every context (context.ts).
        ctx.provide_inner(EventsService);
        ctx.provide_inner(LoggerService);
        ctx.provide_inner(ReflectService);
        ctx.provide_inner(RegistryService);
        ctx
    }

    /// Returns the fiber associated with this context.
    pub fn fiber(&self) -> &Rc<Fiber> {
        &self.inner.fiber
    }

    /// Looks up a service by name.
    ///
    /// Returns `None` when no provider is visible from this context (it never
    /// panics on a missing entry).
    pub fn get_str(&self, name: &str) -> Option<Rc<dyn Any>> {
        let label = self.inner.isolate.lookup(name)?;
        let store = self.inner.store.borrow();
        let entry = store.by_label.get(&label)?;
        if entry.fiber.state.get() == FiberState::Disposed {
            return None;
        }
        Some(entry.value.clone())
    }

    /// Looks up a typed service.
    pub fn get<S: Service>(&self) -> Option<Rc<S>> {
        self.get_str(S::NAME)?.downcast::<S>().ok()
    }

    /// Registers a service by name.
    ///
    /// The value is stored under the isolate label resolved for `name`; when
    /// no label exists yet, one is created on the root layer (mirroring
    /// `ctx.root[symbols.isolate][name] ??= Symbol(name)` in the TS source).
    /// The returned disposer removes the registration.
    pub fn provide_str(&self, name: &str, value: Rc<dyn Any>) -> Result<Disposer, String> {
        let label = self.ensure_label(name);
        let mut store = self.inner.store.borrow_mut();
        if store.by_label.contains_key(&label) {
            let existing = &store.by_label[&label];
            return Err(format!(
                "service \"{}\" has been registered at <{}>",
                existing.name, existing.fiber.name
            ));
        }
        store.by_label.insert(
            label.clone(),
            StoreEntry {
                name: name.to_string(),
                value,
                fiber: self.inner.fiber.clone(),
            },
        );
        drop(store);

        let store = self.inner.store.clone();
        Ok(Box::new(move || {
            store.borrow_mut().by_label.remove(&label);
        }))
    }

    /// Registers a typed service.
    pub fn provide<S: Service>(&self, value: Rc<S>) -> Result<Disposer, String> {
        self.provide_str(S::NAME, value)
    }

    /// Returns a new context with an additional isolate layer.
    ///
    /// The child shares the store and fiber of its parent, but maps `name` to
    /// `label`. Services provided by the child are invisible to the parent,
    /// while services already visible to the parent remain visible to the
    /// child unless the child isolates the same name with a different label.
    pub fn isolate(&self, name: &str, label: Label) -> Context {
        let layer = Rc::new(IsolateLayer {
            entries: RefCell::new(HashMap::from([(name.to_string(), label)])),
            parent: Some(self.inner.isolate.clone()),
        });
        self.spawn(layer, self.inner.intercept.clone())
    }

    /// Returns a new context with an additional intercept layer.
    ///
    /// Config entries registered here override entries from parent layers
    /// when [`Context::resolve_config`] merges the chain.
    pub fn intercept<C: Config>(&self, name: &str, config: C) -> Context {
        let layer = Rc::new(InterceptLayer {
            entries: RefCell::new(HashMap::from([(
                name.to_string(),
                Rc::new(config) as Rc<dyn Any>,
            )])),
            parent: Some(self.inner.intercept.clone()),
        });
        self.spawn(self.inner.isolate.clone(), layer)
    }

    /// Returns a new context carrying arbitrary metadata.
    ///
    /// Metadata entries are appended to those of the parent; lookups prefer
    /// the nearest entry with the same key.
    pub fn extend(&self, meta: &[(&str, Rc<dyn Any>)]) -> Context {
        let ctx = self.spawn(self.inner.isolate.clone(), self.inner.intercept.clone());
        let mut entries = self.inner.meta.borrow().clone();
        for (key, value) in meta {
            entries.push((key.to_string(), value.clone()));
        }
        *ctx.inner.meta.borrow_mut() = entries;
        ctx
    }

    /// Returns a metadata value previously attached via [`Context::extend`].
    pub fn meta<T: Any>(&self, key: &str) -> Option<Rc<T>> {
        for (k, value) in self.inner.meta.borrow().iter().rev() {
            if k == key {
                return value.clone().downcast::<T>().ok();
            }
        }
        None
    }

    /// Resolves the merged config for `name` along the intercept chain.
    ///
    /// Merge order is `base`, then parent layers (bottom-up), then the nearest
    /// layer, then `head`; later entries override earlier ones (mirrors
    /// `Service::resolveConfig` in the TS reference).
    pub fn resolve_config<C: Config>(&self, name: &str, base: Option<&C>, head: Option<&C>) -> C {
        let mut configs: Vec<Rc<dyn Any>> = Vec::new();
        let mut layer = Some(self.inner.intercept.clone());
        while let Some(current) = layer {
            if let Some(config) = current.entries.borrow().get(name) {
                configs.push(config.clone());
            }
            layer = current.parent.clone();
        }
        configs.reverse();

        let mut result = C::default();
        if let Some(base) = base {
            result = result.merge(base);
        }
        for config in configs {
            let config = config
                .downcast_ref::<C>()
                .expect("intercept config type mismatch");
            result = result.merge(config);
        }
        if let Some(head) = head {
            result = result.merge(head);
        }
        result
    }

    fn spawn(&self, isolate: Rc<IsolateLayer>, intercept: Rc<InterceptLayer>) -> Context {
        Context {
            inner: Rc::new(ContextInner {
                isolate,
                intercept,
                store: self.inner.store.clone(),
                fiber: self.inner.fiber.clone(),
                meta: RefCell::new(self.inner.meta.borrow().clone()),
            }),
        }
    }

    fn ensure_label(&self, name: &str) -> Label {
        if let Some(label) = self.inner.isolate.lookup(name) {
            return label;
        }
        let id = NEXT_LABEL_ID.fetch_add(1, Ordering::Relaxed);
        let label: Label = Rc::from(format!("{name}#{id}"));
        self.inner.isolate.bottom().insert(name, label.clone());
        label
    }

    fn provide_inner<S: Service>(&self, value: S) {
        // Framework services never collide and live for the whole chain.
        drop(
            self.provide_str(S::NAME, Rc::new(value))
                .expect("framework service must register"),
        );
    }
}

impl Default for Context {
    fn default() -> Self {
        Context::new()
    }
}
