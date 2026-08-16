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

use crate::error::ConfigValidator;
use crate::events::{EventCallback, EventFilter, EventOptions, ParallelError};
use crate::fiber::{CordisError, EffectHandle, Fiber, FiberState};
use crate::logger::Logger;
use crate::registry::{Plugin, RegistryService};
use crate::service::{ApplyFn, Config, Effect, Service};
use crate::{EventsService, LoggerService, ReflectService};

static NEXT_LABEL_ID: AtomicU64 = AtomicU64::new(1);

/// A service availability check (`Service::check` in the TS reference).
pub type ServiceCheck = Rc<dyn Fn(&Context) -> bool>;

/// A mixin getter: resolves the associated value for the source service.
pub type MixinGet = Rc<dyn Fn(&Context) -> Option<Rc<dyn Any>>>;

/// A mixin setter.
pub type MixinSet = Rc<dyn Fn(&Context, Rc<dyn Any>)>;

/// A callable-service invocation handler (`[Service.invoke]` in the TS
/// reference).
pub type InvokeFn = Rc<dyn Fn(&Context, Option<&Rc<dyn Any>>) -> Option<Rc<dyn Any>>>;

/// A registered accessor (`Property.Accessor` in reflect.ts).
pub struct MixinAccessor {
    /// Resolves the value.
    pub get: MixinGet,
    /// Optionally writes the value.
    pub set: Option<MixinSet>,
}

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
    pub check: Option<ServiceCheck>,
    pub invoke: Option<InvokeFn>,
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
pub(crate) struct ContextInner {
    pub isolate: Rc<IsolateLayer>,
    pub intercept: Rc<InterceptLayer>,
    pub store: Rc<RefCell<Store>>,
    pub meta: RefCell<Vec<(String, Rc<dyn Any>)>>,
    pub props: RefCell<HashMap<String, Rc<MixinAccessor>>>,
}

impl std::fmt::Debug for ContextInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextInner")
            .field("isolate", &self.isolate)
            .field("intercept", &self.intercept)
            .field("store", &self.store)
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
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
        write!(f, "Context <{}>", self.fiber.name())
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
            props: RefCell::new(HashMap::new()),
        });
        let fiber = Fiber::root(inner.clone());
        let ctx = Context { inner, fiber };

        // Framework services are visible on every context (context.ts).
        ctx.provide_inner(EventsService::default());
        ctx.provide_inner(LoggerService::default());
        ctx.provide_inner(ReflectService);
        ctx.provide_inner(RegistryService::default());
        // context.ts clears the root fiber's disposables after framework
        // services are registered, so they don't surface as user effects.
        ctx.fiber.disposables.borrow_mut().clear();
        ctx
    }

    /// Returns the fiber associated with this context.
    pub fn fiber(&self) -> &Rc<Fiber> {
        &self.fiber
    }

    /// Resolves the isolate label for `name` along this context's chain.
    pub fn isolate_label(&self, name: &str) -> Option<Label> {
        self.inner.isolate_label(name)
    }

    /// Returns a context sharing this context's state but bound to `fiber`.
    pub fn with_fiber(&self, fiber: Rc<Fiber>) -> Context {
        Context {
            inner: self.inner.clone(),
            fiber,
        }
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

    /// Whether any visible store entry with `name` exists (used by the
    /// loader to detect provider-side re-scoping).
    pub fn provides(&self, name: &str) -> bool {
        self.inner
            .store
            .borrow()
            .by_label
            .values()
            .any(|entry| entry.name == name)
    }

    /// Moves a store entry from `old_label` to `new_label` when its provider
    /// fiber matches `provider` (mirrors the loader's service migration).
    pub fn migrate_label_if(
        &self,
        name: &str,
        old_label: &Label,
        new_label: &Label,
        provider: &Rc<Fiber>,
    ) -> bool {
        let mut store = self.inner.store.borrow_mut();
        let Some(entry) = store.by_label.get(old_label).cloned() else {
            return false;
        };
        if entry.name != name {
            return false;
        }
        let owned_by = entry
            .fiber
            .upgrade()
            .map(|fiber| Rc::ptr_eq(&fiber, provider))
            .unwrap_or(false);
        if !owned_by || store.by_label.contains_key(new_label) {
            return false;
        }
        store.by_label.remove(old_label);
        store.by_label.insert(new_label.clone(), entry);
        true
    }

    /// Strict dynamic access (mirrors the throwing `ctx[name]` access in the
    /// TS reference).
    ///
    /// Returns an error with the TS-compatible message when the property is
    /// missing or the context is inactive.
    pub fn get_str_strict(&self, name: &str) -> Result<Rc<dyn Any>, String> {
        if self.fiber.uid.get().is_none() {
            return Err(format!(
                "cannot get required service \"{name}\" in inactive context"
            ));
        }
        self.get_str(name)
            .ok_or_else(|| format!("cannot get property \"{name}\" without inject"))
    }

    /// Dynamic set (mirrors `ctx[name] = value` in the TS reference).
    pub fn set_str(&self, name: &str, value: Rc<dyn Any>) -> Result<(), String> {
        let label = self
            .inner
            .isolate
            .lookup(name)
            .ok_or_else(|| format!("cannot set property \"{name}\" without provide"))?;
        let mut store = self.inner.store.borrow_mut();
        match store.by_label.get_mut(&label) {
            Some(entry) => {
                let name = entry.name.clone();
                let fiber = entry.fiber.clone();
                let check = entry.check.clone();
                let invoke = entry.invoke.clone();
                *entry = Rc::new(StoreEntry {
                    name,
                    value,
                    fiber,
                    check,
                    invoke,
                });
                Ok(())
            }
            None => Err(format!("cannot set property \"{name}\" without provide")),
        }
    }

    /// Whether the value is a [`Context`] (mirrors `Context.is(value)`).
    pub fn is_context(value: &dyn Any) -> bool {
        value.is::<Context>()
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
        self.provide_str_with_check(name, value, None)
    }

    /// Registers a service with an explicit availability check.
    pub fn provide_str_with_check(
        &self,
        name: &str,
        value: Rc<dyn Any>,
        check: Option<ServiceCheck>,
    ) -> Result<Rc<EffectHandle>, String> {
        self.provide_inner_impl(name, value, check, None)
    }

    fn provide_inner_impl(
        &self,
        name: &str,
        value: Rc<dyn Any>,
        check: Option<ServiceCheck>,
        invoke: Option<InvokeFn>,
    ) -> Result<Rc<EffectHandle>, String> {
        self.fiber.assert_active().map_err(|e| e.message)?;
        {
            let props = self.inner.props.borrow();
            if props.contains_key(name) {
                return Err(format!("property \"{name}\" is already declared"));
            }
        }
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
                        check: check.clone(),
                        invoke: invoke.clone(),
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
        let check = {
            let value = value.clone();
            Rc::new(move |ctx: &Context| value.check(ctx)) as ServiceCheck
        };
        let invoke = {
            let value = value.clone();
            Rc::new(move |ctx: &Context, init: Option<&Rc<dyn Any>>| value.invoke(ctx, init))
                as InvokeFn
        };
        self.provide_inner_impl(S::NAME, value, Some(check), Some(invoke))
    }

    /// Invokes a callable service (mirrors `ctx[service](...)`).
    pub fn invoke_str(&self, name: &str, init: Option<Rc<dyn Any>>) -> Option<Rc<dyn Any>> {
        let entry = self.inner.lookup_strict(name)?;
        let invoke = entry.invoke.as_ref()?;
        invoke(self, init.as_ref())
    }

    /// Invokes a typed callable service.
    pub fn invoke<S: Service>(&self, init: Option<Rc<dyn Any>>) -> Option<Rc<dyn Any>> {
        self.invoke_str(S::NAME, init)
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

    /// Returns a context with an empty isolate layer on top (used by the
    /// loader to scope entry services per-realm).
    pub fn with_isolate_layer(&self) -> Context {
        let layer = Rc::new(IsolateLayer {
            entries: RefCell::new(HashMap::new()),
            parent: Some(self.inner.isolate.clone()),
        });
        self.spawn(layer, self.inner.intercept.clone())
    }

    /// Sets an isolate label on this context's top layer.
    pub fn set_isolate(&self, name: &str, label: Label) {
        self.inner
            .isolate
            .entries
            .borrow_mut()
            .insert(name.to_string(), label);
    }

    /// Removes an isolate label from this context's top layer.
    pub fn remove_isolate(&self, name: &str) {
        self.inner.isolate.entries.borrow_mut().remove(name);
    }

    /// Clears all entries on this context's top isolate layer.
    pub fn clear_isolate_layer(&self) {
        self.inner.isolate.entries.borrow_mut().clear();
    }

    /// Returns a context with an empty intercept layer on top.
    pub fn with_intercept_layer(&self) -> Context {
        let layer = Rc::new(InterceptLayer {
            entries: RefCell::new(HashMap::new()),
            parent: Some(self.inner.intercept.clone()),
        });
        self.spawn(self.inner.isolate.clone(), layer)
    }

    /// Sets an intercept config on this context's top layer.
    pub fn set_intercept(&self, name: &str, config: Rc<dyn Any>) {
        self.inner
            .intercept
            .entries
            .borrow_mut()
            .insert(name.to_string(), config);
    }

    /// Removes an intercept config from this context's top layer.
    pub fn remove_intercept(&self, name: &str) {
        self.inner.intercept.entries.borrow_mut().remove(name);
    }

    /// Clears all entries on this context's top intercept layer.
    pub fn clear_intercept_layer(&self) {
        self.inner.intercept.entries.borrow_mut().clear();
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

    /// Registers mixin accessors (mirrors `ctx.mixin(source, map)`).
    ///
    /// Each `(key, target_name)` entry makes `ctx.resolve_assoc(source, key)`
    /// resolve the target service or value.
    pub fn mixin(&self, source: &str, entries: &[(&str, &str)]) -> Result<(), String> {
        let accessors: Vec<(String, Rc<MixinAccessor>)> = entries
            .iter()
            .map(|(key, target_name)| {
                let target_name = target_name.to_string();
                let get: MixinGet = Rc::new(move |ctx| ctx.get_str(&target_name));
                (key.to_string(), Rc::new(MixinAccessor { get, set: None }))
            })
            .collect();
        self.register_accessors(source, accessors)
    }

    /// Registers mixin accessors with custom get/set closures.
    pub fn mixin_with(
        &self,
        source: &str,
        entries: &[(&str, MixinAccessor)],
    ) -> Result<(), String> {
        let accessors = entries
            .iter()
            .map(|(key, accessor)| {
                let get = accessor.get.clone();
                let set = accessor.set.clone();
                (key.to_string(), Rc::new(MixinAccessor { get, set }))
            })
            .collect();
        self.register_accessors(source, accessors)
    }

    fn register_accessors(
        &self,
        source: &str,
        entries: Vec<(String, Rc<MixinAccessor>)>,
    ) -> Result<(), String> {
        let mut props = self.inner.props.borrow_mut();
        for (key, accessor) in entries {
            let full = format!("{source}.{key}");
            if props.contains_key(&key) {
                return Err(format!("property \"{full}\" is already declared"));
            }
            props.insert(key, accessor);
        }
        Ok(())
    }

    /// Resolves an associated value `source.key` (mirrors the property
    /// access `ctx[source].key` in the TS reference).
    pub fn resolve_assoc(&self, source: &str, key: &str) -> Option<Rc<dyn Any>> {
        let _ = source;
        let accessor = self.inner.props.borrow().get(key)?.clone();
        (accessor.get)(self)
    }

    /// Sets an associated value `source.key`.
    pub fn set_assoc(&self, source: &str, key: &str, value: Rc<dyn Any>) -> Result<(), String> {
        let full = format!("{source}.{key}");
        let accessor = {
            let props = self.inner.props.borrow();
            props
                .get(key)
                .cloned()
                .ok_or_else(|| format!("cannot set property \"{full}\" without provide"))?
        };
        match &accessor.set {
            Some(set) => {
                set(self, value);
                Ok(())
            }
            None => Err(format!("cannot set property \"{full}\" without provide")),
        }
    }

    /// Registers a plugin on this context (see [`RegistryService::plugin`]).
    pub fn plugin(&self, plugin: &Plugin, config: Option<Rc<dyn Any>>) -> Rc<Fiber> {
        self.plugin_with_validator(plugin, config, None)
    }

    /// Registers a plugin via the registry service (used by the loader).
    pub fn registry_plugin(&self, plugin: &Plugin, config: Option<Rc<dyn Any>>) -> Rc<Fiber> {
        let registry = self
            .get::<RegistryService>()
            .expect("registry service must be present");
        registry.plugin(self, plugin, config)
    }

    /// Registers a plugin with a config validator (story card B12).
    pub fn plugin_with_validator(
        &self,
        plugin: &Plugin,
        config: Option<Rc<dyn Any>>,
        validator: Option<ConfigValidator>,
    ) -> Rc<Fiber> {
        let registry = self
            .get::<RegistryService>()
            .expect("registry service must be present");
        registry.plugin_with_validator(self, plugin, config, validator)
    }

    /// Registers an inject callback (a plugin whose only role is consuming
    /// the declared dependencies).
    pub fn inject(&self, deps: &[&str], callback: ApplyFn) -> Rc<Fiber> {
        let plugin = Plugin {
            name: None,
            inject: deps.iter().map(|s| (s.to_string(), None)).collect(),
            apply: callback,
        };
        self.plugin(&plugin, None)
    }

    /// Registers an effect on this context's fiber (mirrors `ctx.effect`).
    pub fn effect<F>(
        &self,
        execute: F,
        label: &str,
    ) -> Result<Rc<EffectHandle>, crate::fiber::CordisError>
    where
        F: FnOnce() -> Effect,
    {
        self.fiber.effect(execute, label)
    }

    /// Registers an event listener (mirrors `ctx.on`).
    pub fn on(
        &self,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        self.get::<EventsService>()
            .expect("events service")
            .on(self, event, callback, options)
    }

    /// Returns a logger bound to this context (mirrors `ctx.logger()`).
    pub fn logger(&self) -> Logger {
        Logger::new(self, None)
    }

    /// Registers a listener with an attached filter.
    pub fn on_filtered(
        &self,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
        filter: crate::events::ListenerFilter,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        self.get::<EventsService>()
            .expect("events service")
            .on_filtered(self, event, callback, options, filter)
    }

    /// Registers a one-shot event listener (mirrors `ctx.once`).
    pub fn once(
        &self,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        self.get::<EventsService>()
            .expect("events service")
            .once(self, event, callback, options)
    }

    /// Emits an event (mirrors `ctx.emit`).
    pub fn emit(&self, event: &str, args: &[Rc<dyn Any>]) {
        self.get::<EventsService>()
            .expect("events service")
            .emit(self, event, args);
    }

    /// Emits with a filter (mirrors `ctx.emit(thisArg, name, ...)`).
    pub fn emit_with(&self, event: &str, args: &[Rc<dyn Any>], this_arg: &dyn EventFilter) {
        self.get::<EventsService>()
            .expect("events service")
            .emit_with(self, event, args, Some(this_arg));
    }

    /// Runs listeners concurrently (mirrors `ctx.parallel`).
    pub async fn parallel(
        &self,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<(), ParallelError> {
        self.get::<EventsService>()
            .expect("events service")
            .parallel(self, event, args, this_arg)
            .await
    }

    /// Runs listeners sequentially (mirrors `ctx.serial`).
    pub async fn serial(
        &self,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn std::error::Error>> {
        self.get::<EventsService>()
            .expect("events service")
            .serial(self, event, args, this_arg)
            .await
    }

    /// Runs listeners synchronously with bail semantics (mirrors `ctx.bail`).
    pub fn bail(
        &self,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn std::error::Error>> {
        self.get::<EventsService>()
            .expect("events service")
            .bail(self, event, args, this_arg)
    }

    /// Runs listeners in a waterfall chain (mirrors `ctx.waterfall`).
    pub fn waterfall(
        &self,
        event: &str,
        args: &[Rc<dyn Any>],
        tail: crate::events::WaterfallNext,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn std::error::Error>> {
        self.get::<EventsService>()
            .expect("events service")
            .waterfall(self, event, args, tail)
    }

    /// Notifies fibers that depend on `name` (mirrors `ReflectService.notify`).
    pub fn notify(&self, name: &str) -> Vec<Rc<Fiber>> {
        let Some(registry) = self.get::<RegistryService>() else {
            return Vec::new();
        };
        registry.notify(name, self)
    }

    /// Notifies fibers whose isolate label for `name` matches one of
    /// `labels` (used by the loader's realm migration).
    pub fn notify_with_labels(&self, name: &str, labels: &[Label]) -> Vec<Rc<Fiber>> {
        let Some(registry) = self.get::<RegistryService>() else {
            return Vec::new();
        };
        registry.notify_with_labels(name, labels)
    }

    fn spawn(&self, isolate: Rc<IsolateLayer>, intercept: Rc<InterceptLayer>) -> Context {
        Context {
            inner: Rc::new(ContextInner {
                isolate,
                intercept,
                store: self.inner.store.clone(),
                meta: RefCell::new(self.inner.meta.borrow().clone()),
                props: self.inner.props.clone(),
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
