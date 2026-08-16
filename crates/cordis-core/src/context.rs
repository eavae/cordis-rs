//! Context: the object every plugin receives.
//!
//! A [`Context`] owns a shareable [`Fiber`], a shared service
//! store and two immutable chain layers:
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

use crate::fiber::{EffectHandle, Fiber, FiberState};
use crate::registry::{Plugin, RegistryService};
use crate::service::{ApplyFn, Config, Effect, Service};
use crate::{EventsService, LoggerService, ReflectService};

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
    pub(crate) entries: RefCell<HashMap<String, Rc<dyn Any>>>,
    pub(crate) parent: Option<Rc<InterceptLayer>>,
}

/// An entry of the shared service store.
pub(crate) struct StoreEntry {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub fiber: std::rc::Weak<Fiber>,
}

impl std::fmt::Debug for StoreEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreEntry")
            .field("name", &self.name)
            .field("fiber_uid", &self.fiber.upgrade().map(|f| f.uid.get()))
            .finish()
    }
}

/// The service store shared by a whole context chain.
#[derive(Debug, Default)]
pub(crate) struct Store {
    pub(crate) by_label: HashMap<Label, Rc<StoreEntry>>,
}

/// Shared inner state of a [`Context`].
#[derive(Debug)]
pub(crate) struct ContextInner {
    pub isolate: Rc<IsolateLayer>,
    pub intercept: Rc<InterceptLayer>,
    pub store: Rc<RefCell<Store>>,
    pub meta: RefCell<Vec<(String, Rc<dyn Any>)>>,
}

impl ContextInner {
    /// Resolves the isolate label for `name` along the chain.
    pub(crate) fn isolate_label(&self, name: &str) -> Option<Label> {
        self.isolate.lookup(name)
    }

    /// Strict store lookup: the entry must exist and its fiber must be
    /// `ACTIVE` (mirrors `_getImpl(name, true)` in reflect.ts).
    pub(crate) fn lookup_strict(&self, name: &str) -> Option<Rc<StoreEntry>> {
        let label = self.isolate.lookup(name)?;
        let entry = self.store.borrow().by_label.get(&label)?.clone();
        let active = entry
            .fiber
            .upgrade()
            .map(|fiber| fiber.state.get() == FiberState::Active)
            .unwrap_or(false);
        if active { Some(entry) } else { None }
    }

    /// Typed service lookup by name.
    pub(crate) fn get_service<S: Service>(&self, name: &str) -> Option<Rc<S>> {
        let entry = self.lookup_strict(name)?;
        entry.value.clone().downcast::<S>().ok()
    }
}

/// The core object handed to plugins.
///
/// A context is intentionally `!Send`: the whole runtime is single-threaded
/// and uses `Rc`/`RefCell` without locks (difficulty 3 decision).
#[derive(Clone)]
pub struct Context {
    pub(crate) inner: Rc<ContextInner>,
    pub(crate) fiber: Rc<Fiber>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("fiber", &self.fiber)
            .finish()
    }
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
        let inner = Rc::new(ContextInner {
            isolate,
            intercept,
            store,
            meta: RefCell::new(Vec::new()),
        });
        let fiber = Fiber::root(inner.clone());
        let ctx = Context { inner, fiber };

        // Framework services are visible on every context (context.ts).
        ctx.provide_inner(EventsService::default());
        ctx.provide_inner(LoggerService::default());
        ctx.provide_inner(ReflectService);
        ctx.provide_inner(RegistryService::default());
        ctx
    }

    /// Returns the fiber associated with this context.
    pub fn fiber(&self) -> &Rc<Fiber> {
        &self.fiber
    }

    /// Looks up a service by name.
    ///
    /// Returns `None` when no active provider is visible from this context
    /// (it never panics on a missing entry).
    pub fn get_str(&self, name: &str) -> Option<Rc<dyn Any>> {
        self.inner
            .lookup_strict(name)
            .map(|entry| entry.value.clone())
    }

    /// Looks up a typed service.
    pub fn get<S: Service>(&self) -> Option<Rc<S>> {
        self.get_str(S::NAME)?.downcast::<S>().ok()
    }

    /// Registers a service by name through the current fiber's effect system.
    ///
    /// The value is stored under the isolate label resolved for `name`; when
    /// no label exists yet, one is created on the root layer (mirroring
    /// `ctx.root[symbols.isolate][name] ??= Symbol(name)` in the TS source).
    /// The returned handle disposes the registration and notifies consumers.
    pub fn provide_str(&self, name: &str, value: Rc<dyn Any>) -> Result<Rc<EffectHandle>, String> {
        self.fiber.assert_active().map_err(|e| e.message)?;
        let label = self.ensure_label(name);
        {
            let store = self.inner.store.borrow();
            if let Some(existing) = store.by_label.get(&label) {
                return Err(format!(
                    "service \"{}\" has been registered at <{}>",
                    existing.name,
                    existing
                        .fiber
                        .upgrade()
                        .map(|f| f.name())
                        .unwrap_or_else(|| "?".to_string())
                ));
            }
        }

        let ctx = self.clone();
        let name = name.to_string();
        let effect_label = format!("ctx.provide({name:?})");
        let handle = self
            .fiber
            .effect(
                move || {
                    let entry = Rc::new(StoreEntry {
                        name: name.clone(),
                        value,
                        fiber: Rc::downgrade(&ctx.fiber),
                    });
                    ctx.inner
                        .store
                        .borrow_mut()
                        .by_label
                        .insert(label.clone(), entry.clone());
                    ctx.fiber.resolved.borrow_mut().insert(name.clone(), entry);
                    if ctx.fiber.state.get() == FiberState::Active {
                        ctx.notify(&name);
                    }
                    let ctx = ctx.clone();
                    Effect::Disposer(Box::new(move || {
                        let ctx = ctx.clone();
                        let name = name.clone();
                        let label = label.clone();
                        Box::pin(async move {
                            ctx.inner.store.borrow_mut().by_label.remove(&label);
                            let fibers = ctx.notify(&name);
                            // TS awaits the affected fibers here; B8 refines the
                            // notification ordering. For B2 the removal and
                            // notify are synchronous, which matches the fiber
                            // spec expectations under fake timers.
                            let _ = fibers;
                            ctx.fiber.resolved.borrow_mut().remove(&name);
                            Ok(())
                        })
                    }))
                },
                &effect_label,
            )
            .map_err(|e| e.message)?;
        Ok(handle)
    }

    /// Registers a typed service.
    pub fn provide<S: Service>(&self, value: Rc<S>) -> Result<Rc<EffectHandle>, String> {
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

    /// Registers a plugin on this context (see [`RegistryService::plugin`]).
    pub fn plugin(&self, plugin: &Plugin, config: Option<Rc<dyn Any>>) -> Rc<Fiber> {
        let registry = self
            .get::<RegistryService>()
            .expect("registry service must be present");
        registry.plugin(self, plugin, config)
    }

    /// Registers an inject callback (a plugin whose only role is consuming
    /// the declared dependencies).
    pub fn inject(&self, deps: &[&str], callback: ApplyFn) -> Rc<Fiber> {
        let plugin = Plugin {
            name: None,
            inject: deps.iter().map(|s| s.to_string()).collect(),
            apply: callback,
        };
        self.plugin(&plugin, None)
    }

    /// Notifies fibers that depend on `name` (mirrors `ReflectService.notify`).
    pub(crate) fn notify(&self, name: &str) -> Vec<Rc<Fiber>> {
        let Some(registry) = self.get::<RegistryService>() else {
            return Vec::new();
        };
        registry.notify(name, self)
    }

    fn spawn(&self, isolate: Rc<IsolateLayer>, intercept: Rc<InterceptLayer>) -> Context {
        Context {
            inner: Rc::new(ContextInner {
                isolate,
                intercept,
                store: self.inner.store.clone(),
                meta: RefCell::new(self.inner.meta.borrow().clone()),
            }),
            fiber: self.fiber.clone(),
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
