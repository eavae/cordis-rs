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
use std::ops::Deref;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::ConfigValidator;
use crate::events::{EventCallback, EventFilter, EventOptions, ParallelError, WaterfallNext};
use crate::fiber::{CordisError, EffectHandle, Fiber, FiberState};
use crate::logger::Logger;
use crate::registry::{Plugin, RegistryService};
use crate::service::{ApplyFn, Config, Effect, Service, sync_disposer};
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
pub type InvokeFn = Rc<dyn Fn(&ShadowContext, Option<&Rc<dyn Any>>) -> Option<Rc<dyn Any>>>;

/// A registered accessor (`Property.Accessor` in reflect.ts).
pub struct MixinAccessor {
    /// Resolves the value.
    pub get: MixinGet,
    /// Optionally writes the value.
    pub set: Option<MixinSet>,
}

/// The concrete store error reported by the `internal/set` waterfall tail
/// (story card B15): without it, a rejected write would always surface the
/// generic "without provide" message instead of the real reason.
struct SetError(String);

/// A service label. Labels compare by value: contexts isolated with the same
/// label share the same service instance (mirrors `Symbol('name')` equality
/// in the TS reference).
pub type Label = Rc<str>;

/// One immutable layer of the isolate chain.
#[derive(Debug, Default)]
pub(crate) struct IsolateLayer {
    entries: RefCell<HashMap<String, Label>>,
    parent: RefCell<Option<Rc<IsolateLayer>>>,
}

impl IsolateLayer {
    fn lookup(&self, name: &str) -> Option<Label> {
        if let Some(label) = self.entries.borrow().get(name) {
            return Some(label.clone());
        }
        self.parent.borrow().as_ref()?.lookup(name)
    }

    /// Returns the bottom-most (root) layer of the chain.
    fn bottom(self: &Rc<IsolateLayer>) -> Rc<IsolateLayer> {
        let mut layer = self.clone();
        loop {
            let next = layer.parent.borrow().clone();
            match next {
                Some(parent) => layer = parent,
                None => return layer,
            }
        }
    }

    fn insert(&self, name: &str, label: Label) {
        self.entries.borrow_mut().insert(name.to_string(), label);
    }
}

/// One immutable layer of the intercept chain.
#[derive(Debug, Default)]
pub(crate) struct InterceptLayer {
    pub(crate) entries: RefCell<HashMap<String, Rc<dyn Any>>>,
    pub(crate) parent: RefCell<Option<Rc<InterceptLayer>>>,
}

/// An entry of the shared service store.
pub(crate) struct StoreEntry {
    pub name: String,
    pub value: Rc<dyn Any>,
    pub fiber: std::rc::Weak<Fiber>,
    /// The inner state of the context on which the service was provided
    /// (the JS `symbols.shadow`). Only the inner is kept: holding a full
    /// [`Context`] here would strongly pin the provider fiber and create an
    /// `Rc` cycle through `Fiber::resolved`.
    pub(crate) shadow_inner: Rc<ContextInner>,
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
    /// Shared accessor table for the whole context tree (mirrors the single
    /// `ReflectService.props` in the TS reference; accessors registered by
    /// any fiber are visible tree-wide).
    pub props: Rc<RefCell<HashMap<String, Rc<MixinAccessor>>>>,
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

    /// Non-strict store lookup: the entry must exist, but the provider fiber
    /// need not be `ACTIVE`. Framework services (events/logger/reflect/
    /// registry) stay reachable even while their fiber is unloading, mirroring
    /// the TS prototype properties.
    pub(crate) fn lookup_non_strict(&self, name: &str) -> Option<Rc<StoreEntry>> {
        let label = self.isolate.lookup(name)?;
        self.store.borrow().by_label.get(&label).cloned()
    }

    /// Typed service lookup by name.
    pub(crate) fn get_service<S: Service>(&self, name: &str) -> Option<Rc<S>> {
        let entry = self.lookup_strict(name)?;
        entry.value.clone().downcast::<S>().ok()
    }

    /// Non-strict typed lookup (see [`ContextInner::lookup_non_strict`]).
    pub(crate) fn get_service_non_strict<S: Service>(&self, name: &str) -> Option<Rc<S>> {
        let entry = self.lookup_non_strict(name)?;
        entry.value.clone().downcast::<S>().ok()
    }
}

/// The recorded shadow of a registered service (mirrors the JS
/// `symbols.shadow` symbol).
///
/// In the TS reference the context a service belongs to is injected
/// implicitly through a Proxy (`this.ctx[symbols.shadow]`). The Rust port
/// (story card B10) passes contexts explicitly instead, and records the
/// shadow here so the information stays queryable without hidden state: the
/// service's own context is the one it was provided on, and the provider
/// fiber is the fiber that registered it.
#[derive(Clone, Debug)]
pub struct ServiceShadow {
    /// The service name (its [`Service::NAME`]).
    pub name: String,
    /// The context on which the service was provided. Resolving services
    /// through this context follows the provider's isolate and intercept
    /// chains.
    pub ctx: Context,
    /// The provider fiber (weak, mirroring how the TS runtime keeps fiber
    /// references in `Message`).
    pub fiber: std::rc::Weak<Fiber>,
}

/// The service-method context (the explicit counterpart of `this.ctx`
/// inside a JS service method).
///
/// In the TS reference, `this.ctx` inside a service method is a hybrid: its
/// own `symbols.shadow` points at the service's own registration context
/// (used for dependency resolution), while its prototype chain points at the
/// caller's context (used for intercept, fiber and other chain reads).
/// [`ShadowContext`] models the same split explicitly:
///
/// - dependency reads ([`get_str`](Self::get_str), [`get`](Self::get),
///   [`has_str`](Self::has_str), ...) resolve through the service's own
///   shadow context (`own`);
/// - everything else dereferences to the caller's context (`caller`), so
///   `resolve_config`, `fiber`, `plugin`, `effect`, events and other
///   `Context` APIs behave as if called from the caller's scope.
///
/// The framework constructs a [`ShadowContext`] for callable invocations
/// ([`Context::invoke_str`]); callers of regular service methods construct
/// one explicitly (typically with [`Self::for_service`] and the callee's
/// shadow from [`Context::shadow_of`]).
#[derive(Clone, Debug)]
pub struct ShadowContext {
    /// The service's own registration context (its shadow).
    own: Context,
    /// The caller's context (the original access chain).
    caller: Context,
}

impl ShadowContext {
    /// Creates a service-method context from the service's own shadow
    /// context and the caller's context.
    pub fn new(own: Context, caller: Context) -> ShadowContext {
        ShadowContext { own, caller }
    }

    /// The service's own shadow context (dependency-resolution scope).
    pub fn own(&self) -> &Context {
        &self.own
    }

    /// The caller's context (intercept / fiber / chain-read scope).
    pub fn caller(&self) -> &Context {
        &self.caller
    }

    /// Returns the context for calling into another service from this
    /// method: the callee's shadow becomes `own`, while the caller chain
    /// stays unchanged (mirrors the JS trace, where each service hop only
    /// replaces the shadow and keeps the original access chain).
    pub fn for_service(&self, next_own: Context) -> ShadowContext {
        ShadowContext {
            own: next_own,
            caller: self.caller.clone(),
        }
    }

    /// Dynamic dependency read through the service's own scope (mirrors
    /// `this.ctx[name]` in the TS reference).
    pub fn get_str(&self, name: &str) -> Option<Rc<dyn Any>> {
        self.own.get_str(name)
    }

    /// Strict dynamic dependency read through the service's own scope.
    pub fn get_str_strict(&self, name: &str) -> Result<Rc<dyn Any>, String> {
        self.own.get_str_strict(name)
    }

    /// Non-strict dynamic dependency read through the service's own scope.
    pub fn get_str_non_strict(&self, name: &str) -> Option<Rc<dyn Any>> {
        self.own.get_str_non_strict(name)
    }

    /// Typed dependency read through the service's own scope.
    pub fn get<S: Service>(&self) -> Option<Rc<S>> {
        self.own.get::<S>()
    }

    /// Whether `name` resolves as a property in the service's own scope.
    pub fn has_str(&self, name: &str) -> bool {
        self.own.has_str(name)
    }

    /// Whether any store entry with `name` exists in the service's own
    /// scope.
    pub fn provides(&self, name: &str) -> bool {
        self.own.provides(name)
    }

    /// The recorded shadow of a service visible from the service's own
    /// scope.
    pub fn shadow_of(&self, name: &str) -> Option<ServiceShadow> {
        self.own.shadow_of(name)
    }

    /// Invokes a callable service from this method (mirrors
    /// `this.ctx[name](...)`): the callable is resolved through the
    /// service's own scope and the invocation receives this method's caller
    /// chain.
    pub fn invoke_str(&self, name: &str, init: Option<Rc<dyn Any>>) -> Option<Rc<dyn Any>> {
        let entry = self.own.inner.lookup_strict(name)?;
        let invoke = entry.invoke.as_ref()?;
        let own = Context {
            inner: entry.shadow_inner.clone(),
            fiber: entry.fiber.upgrade()?,
        };
        invoke(
            &ShadowContext {
                own,
                caller: self.caller.clone(),
            },
            init.as_ref(),
        )
    }

    /// Typed variant of [`ShadowContext::invoke_str`].
    pub fn invoke<S: Service>(&self, init: Option<Rc<dyn Any>>) -> Option<Rc<dyn Any>> {
        self.invoke_str(S::NAME, init)
    }

    /// Returns a logger bound to this service-method context.
    ///
    /// The name is resolved like the TS traceable logger: the caller's
    /// intercept chain wins, and the fallback name comes from the service's
    /// own shadow fiber (mirrors `symbols.caller` in the TS reference), so a
    /// service method logs under its own fiber name automatically.
    pub fn logger(&self) -> Logger {
        Logger::traced(self.caller(), self.own(), None)
    }
}

impl Deref for ShadowContext {
    type Target = Context;

    /// Everything except dependency reads behaves as the caller's context:
    /// intercept/fiber/plugin/effect/events all operate in the caller's
    /// scope (mirrors the JS shadow's prototype chain).
    fn deref(&self) -> &Context {
        &self.caller
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
            props: Rc::new(RefCell::new(HashMap::new())),
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

    /// Whether both contexts share the same inner state segment (used by the
    /// loader to identify an entry's own context).
    pub fn shares_inner(&self, other: &Context) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
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
    ///
    /// Dynamic access runs through the `internal/get` waterfall (story card
    /// B14): listeners may override the value or call `next()` to fall back
    /// to the strict store lookup.
    pub fn get_str(&self, name: &str) -> Option<Rc<dyn Any>> {
        let error = format!("cannot get property \"{name}\" without inject");
        let args: Vec<Rc<dyn Any>> = vec![
            Rc::new(self.clone()),
            Rc::new(name.to_string()),
            Rc::new(error),
        ];
        let this = self.clone();
        let name = name.to_string();
        let tail: WaterfallNext = Rc::new(move || {
            this.inner
                .lookup_strict(&name)
                .map(|entry| entry.value.clone())
        });
        self.waterfall("internal/get", &args, tail).ok().flatten()
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

    /// Returns the recorded shadow of a registered service (mirrors reading
    /// `ctx[symbols.shadow]` in the TS reference).
    ///
    /// The shadow is the context the service was provided on: its own
    /// scope, with the provider's isolate and intercept chains. Unlike the
    /// TS proxy, the Rust port never injects this context implicitly —
    /// service methods receive contexts explicitly (story card B10) — so
    /// this query exists to keep the information observable and auditable.
    ///
    /// The lookup is non-strict: the shadow remains available as long as the
    /// entry exists, even while the provider fiber is unloading or failed
    /// (mirrors the service object keeping its `ctx` in JS).
    pub fn shadow_of(&self, name: &str) -> Option<ServiceShadow> {
        let entry = self.inner.lookup_non_strict(name)?;
        let fiber = entry.fiber.upgrade()?;
        Some(ServiceShadow {
            name: entry.name.clone(),
            ctx: Context {
                inner: entry.shadow_inner.clone(),
                fiber: fiber.clone(),
            },
            fiber: entry.fiber.clone(),
        })
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

    /// Moves a store entry from `old_label` to `new_label` when the provider
    /// itself now resolves `new_label` (mirrors the TS isolate patch-context
    /// delimiter guard: the impl's fiber realm moved with it).
    pub fn migrate_label(&self, name: &str, old_label: &Label, new_label: &Label) -> bool {
        let mut store = self.inner.store.borrow_mut();
        let Some(entry) = store.by_label.get(old_label).cloned() else {
            return false;
        };
        if entry.name != name || store.by_label.contains_key(new_label) {
            return false;
        }
        let provider_moved = entry
            .fiber
            .upgrade()
            .map(|fiber| {
                fiber
                    .ctx
                    .isolate_label(name)
                    .map(|label| &label == new_label)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !provider_moved {
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

    /// Reads a service without requiring its provider fiber to be `ACTIVE`
    /// (mirrors `reflect.get(name, false)`).
    ///
    /// Unlike [`Context::get_str`] this bypasses the `internal/get`
    /// waterfall and returns the value of any registered entry, even while
    /// the provider fiber is unloading or failed.
    pub fn get_str_non_strict(&self, name: &str) -> Option<Rc<dyn Any>> {
        self.inner
            .lookup_non_strict(name)
            .map(|entry| entry.value.clone())
    }

    /// Whether `name` resolves as a property: a registered accessor or a
    /// registered service (mirrors the `in` operator / Proxy `has` handler).
    ///
    /// Rust contexts have no dynamic own properties, so the "own property"
    /// source of the TS `has` handler is not applicable.
    pub fn has_str(&self, name: &str) -> bool {
        self.inner.props.borrow().contains_key(name) || self.inner.lookup_non_strict(name).is_some()
    }

    /// Dynamic set (mirrors `ctx[name] = value` in the TS reference).
    ///
    /// Dynamic writes run through the `internal/set` waterfall (story card
    /// B14): listeners may accept the write (bail with `true`) or call
    /// `next()` to fall back to the strict store update.
    ///
    /// The strict store update enforces ownership (story card B15): only the
    /// fiber that provided the service may set its value; otherwise the
    /// `"cannot set property \"{name}\" in multiple fibers"` error is
    /// returned.
    pub fn set_str(&self, name: &str, value: Rc<dyn Any>) -> Result<(), String> {
        let error = format!("cannot set property \"{name}\" without provide");
        let args: Vec<Rc<dyn Any>> = vec![
            Rc::new(self.clone()),
            Rc::new(name.to_string()),
            value.clone(),
            Rc::new(error.clone()),
        ];
        let this = self.clone();
        let name = name.to_string();
        let tail: WaterfallNext = Rc::new(move || match this.set_str_impl(&name, value.clone()) {
            Ok(()) => Some(Rc::new(true)),
            Err(message) => Some(Rc::new(SetError(message))),
        });
        match self.waterfall("internal/set", &args, tail) {
            Ok(Some(result)) if result.downcast_ref::<bool>().copied().unwrap_or(false) => Ok(()),
            Ok(Some(result)) => Err(result
                .downcast_ref::<SetError>()
                .map(|error| error.0.clone())
                .unwrap_or(error)),
            _ => Err(error),
        }
    }

    fn set_str_impl(&self, name: &str, value: Rc<dyn Any>) -> Result<(), String> {
        let label = self
            .inner
            .isolate
            .lookup(name)
            .ok_or_else(|| format!("cannot set property \"{name}\" without provide"))?;
        let mut store = self.inner.store.borrow_mut();
        match store.by_label.get_mut(&label) {
            Some(entry) => {
                // Ownership: only the providing fiber may update the value
                // (mirrors reflect.set "cannot set property in multiple
                // fibers").
                let owned = entry
                    .fiber
                    .upgrade()
                    .is_some_and(|fiber| Rc::ptr_eq(&fiber, &self.fiber));
                if !owned {
                    return Err(format!("cannot set property \"{name}\" in multiple fibers"));
                }
                let name = entry.name.clone();
                let fiber = entry.fiber.clone();
                let shadow_inner = entry.shadow_inner.clone();
                let check = entry.check.clone();
                let invoke = entry.invoke.clone();
                *entry = Rc::new(StoreEntry {
                    name,
                    value,
                    fiber,
                    shadow_inner,
                    check,
                    invoke,
                });
            }
            None => return Err(format!("cannot set property \"{name}\" without provide")),
        }
        drop(store);
        // Notify injectors that the service value changed (story card B15).
        drop(self.notify(name));
        Ok(())
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
                        shadow_inner: ctx.inner.clone(),
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
            Rc::new(move |ctx: &ShadowContext, init: Option<&Rc<dyn Any>>| value.invoke(ctx, init))
                as InvokeFn
        };
        self.provide_inner_impl(S::NAME, value, Some(check), Some(invoke))
    }

    /// Invokes a callable service (mirrors `ctx[service](...)`).
    ///
    /// The invocation receives a [`ShadowContext`] whose `own` is the
    /// callable's recorded shadow (its own registration context) and whose
    /// caller is this context — the faithful counterpart of the JS proxy
    /// injecting `this.ctx` into `[Service.invoke]`.
    pub fn invoke_str(&self, name: &str, init: Option<Rc<dyn Any>>) -> Option<Rc<dyn Any>> {
        let entry = self.inner.lookup_strict(name)?;
        let invoke = entry.invoke.as_ref()?;
        let own = Context {
            inner: entry.shadow_inner.clone(),
            fiber: entry.fiber.upgrade()?,
        };
        invoke(
            &ShadowContext {
                own,
                caller: self.clone(),
            },
            init.as_ref(),
        )
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
            parent: RefCell::new(Some(self.inner.isolate.clone())),
        });
        self.spawn(layer, self.inner.intercept.clone())
    }

    /// Returns a context with an empty isolate layer on top (used by the
    /// loader to scope entry services per-realm).
    pub fn with_isolate_layer(&self) -> Context {
        let layer = Rc::new(IsolateLayer {
            entries: RefCell::new(HashMap::new()),
            parent: RefCell::new(Some(self.inner.isolate.clone())),
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
            parent: RefCell::new(Some(self.inner.intercept.clone())),
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
            parent: RefCell::new(Some(self.inner.intercept.clone())),
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
            layer = current.parent.borrow().clone();
        }
        configs.reverse();

        let mut result = C::default();
        if let Some(base) = base {
            result = result.merge(base);
        }
        for config in configs {
            // Typed configs skip entries of a different type (mirrors the
            // dynamic TS chain, where each service reads what it knows).
            if let Some(config) = config.downcast_ref::<C>() {
                result = result.merge(config);
            }
        }
        if let Some(head) = head {
            result = result.merge(head);
        }
        result
    }

    /// The intercept chain for `name`, nearest layer first (mirrors reading
    /// `ctx[Context.intercept]` and walking its prototype chain).
    pub fn intercept_chain(&self, name: &str) -> Vec<Rc<dyn Any>> {
        let mut result = Vec::new();
        let mut layer = Some(self.inner.intercept.clone());
        while let Some(current) = layer {
            if let Some(config) = current.entries.borrow().get(name) {
                result.push(config.clone());
            }
            layer = current.parent.borrow().clone();
        }
        result
    }

    /// Re-points the top isolate and intercept layers at `new_parent`'s
    /// chains (mirrors `Object.setPrototypeOf(ctx, parent.ctx)` when an entry
    /// moves between groups; the fiber keeps running).
    pub fn reparent(&self, new_parent: &Context) {
        *self.inner.isolate.parent.borrow_mut() = Some(new_parent.inner.isolate.clone());
        *self.inner.intercept.parent.borrow_mut() = Some(new_parent.inner.intercept.clone());
    }

    /// Registers mixin accessors (mirrors `ctx.mixin(source, map)`).
    ///
    /// Each `(key, target_name)` entry makes `ctx.resolve_assoc(source, key)`
    /// resolve the target service or value. The registration is an effect of
    /// the current fiber and is removed when the fiber unloads.
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
    ///
    /// The registration is an effect of the current fiber and is removed
    /// when the fiber unloads.
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

    /// Registers a named accessor (mirrors `ctx.accessor(name, { get, set })`).
    ///
    /// Reads go through [`Context::resolve_assoc`] and writes through
    /// [`Context::set_assoc`]. The registration is an effect of the current
    /// fiber: it is removed when the fiber unloads, and conflicts with an
    /// existing property of the same name are rejected.
    pub fn accessor(
        &self,
        name: &str,
        get: MixinGet,
        set: Option<MixinSet>,
    ) -> Result<Rc<EffectHandle>, String> {
        let this = self.clone();
        let name = name.to_string();
        let label = format!("ctx.accessor({name:?})");
        self.fiber
            .effect(
                move || {
                    // Conflicts with an existing accessor or a same-name
                    // service are rejected (mirrors the TS accessor effect).
                    let exists = this.inner.lookup_non_strict(&name).is_some()
                        || this.inner.props.borrow().contains_key(&name);
                    if exists {
                        return Effect::Error(
                            format!("property \"{name}\" is already declared").into(),
                        );
                    }
                    this.inner
                        .props
                        .borrow_mut()
                        .insert(name.clone(), Rc::new(MixinAccessor { get, set }));
                    let this_for_dispose = this.clone();
                    let name_for_dispose = name.clone();
                    Effect::Disposer(sync_disposer(move || {
                        this_for_dispose
                            .inner
                            .props
                            .borrow_mut()
                            .remove(&name_for_dispose);
                    }))
                },
                &label,
            )
            .map_err(|error| error.message)
    }

    fn register_accessors(
        &self,
        source: &str,
        entries: Vec<(String, Rc<MixinAccessor>)>,
    ) -> Result<(), String> {
        let this = self.clone();
        let source = source.to_string();
        let keys: Vec<String> = entries.iter().map(|(key, _)| key.clone()).collect();
        let label = format!("ctx.mixin({source:?})");
        self.fiber
            .effect(
                move || {
                    let conflict = {
                        let mut props = this.inner.props.borrow_mut();
                        let mut conflict = None;
                        for (key, accessor) in entries {
                            let full = format!("{source}.{key}");
                            if props.contains_key(&key) {
                                conflict = Some(format!("property \"{full}\" is already declared"));
                                break;
                            }
                            props.insert(key, accessor);
                        }
                        conflict
                    };
                    if let Some(message) = conflict {
                        return Effect::Error(message.into());
                    }
                    let this_for_dispose = this.clone();
                    Effect::Disposer(sync_disposer(move || {
                        let mut props = this_for_dispose.inner.props.borrow_mut();
                        for key in keys {
                            props.remove(&key);
                        }
                    }))
                },
                &label,
            )
            .map(|_handle| ())
            .map_err(|error| error.message)
    }

    /// Resolves an associated value `source.key` (mirrors the property
    /// access `ctx[source].key` in the TS reference).
    pub fn resolve_assoc(&self, source: &str, key: &str) -> Option<Rc<dyn Any>> {
        let _ = source;
        let props = self.inner.props.borrow();
        let accessor = props.get(key)?.clone();
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
            is_group: false,
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
        self.events()
            .expect("events service")
            .on(self, event, callback, options)
    }

    /// Returns a logger bound to this context (mirrors `ctx.logger()`).
    pub fn logger(&self) -> Logger {
        Logger::new(self, None)
    }

    /// Resolves the shared events service directly from the store.
    ///
    /// Event dispatch must not resolve through `get_str`'s `internal/get`
    /// waterfall, or a dynamic get would recurse forever through the events
    /// service lookup. The framework services are always reachable, even
    /// while the owning fiber is unloading.
    fn events(&self) -> Option<Rc<EventsService>> {
        self.inner.get_service_non_strict::<EventsService>("events")
    }

    /// Registers a listener with an attached filter.
    pub fn on_filtered(
        &self,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
        filter: crate::events::ListenerFilter,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        self.events()
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
        self.events()
            .expect("events service")
            .once(self, event, callback, options)
    }

    /// Emits an event (mirrors `ctx.emit`).
    pub fn emit(&self, event: &str, args: &[Rc<dyn Any>]) {
        self.events()
            .expect("events service")
            .emit(self, event, args);
    }

    /// Emits with a filter (mirrors `ctx.emit(thisArg, name, ...)`).
    pub fn emit_with(&self, event: &str, args: &[Rc<dyn Any>], this_arg: &dyn EventFilter) {
        self.events()
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
        self.events()
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
        self.events()
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
        self.events()
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
        self.events()
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
