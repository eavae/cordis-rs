//! Context: the object every plugin receives.
//!
//! A [`Context`] owns a shareable [`Fiber`], a shared service store and one
//! shared *overlay* chain.
//!
//! Each overlay layer carries both the isolate labels (service names → labels;
//! services provided in an isolated context are only visible to contexts that
//! share the label) and the intercept overrides (per-service config merged by
//! [`Context::resolve_config`]). Keeping the two maps in a single layer lets
//! an overlay reconfiguration publish one atomic snapshot instead of two
//! independent stores.

use std::any::Any;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::task::Poll;

use arc_swap::ArcSwap;

use crate::error::ConfigValidator;
use crate::events::{
    EventCallback, EventFilter, EventOptions, ParallelError, WaterfallNext, poll_once,
};
use crate::fiber::{CordisError, EffectHandle, Fiber, FiberState};
use crate::logger::Logger;
use crate::registry::{Plugin, RegistryService};
use crate::service::{ApplyFn, BoxError, Config, Effect, Service, sync_disposer};
use crate::{EventsService, LoggerService, ReflectService};

static NEXT_LABEL_ID: AtomicU64 = AtomicU64::new(1);

/// A service availability check (`Service::check` in the TS reference).
pub type ServiceCheck = Arc<dyn Fn(&Context) -> bool + Send + Sync>;

/// A mixin getter: resolves the associated value for the source service.
pub type MixinGet = Arc<dyn Fn(&Context) -> Option<Arc<dyn Any + Send + Sync>> + Send + Sync>;

/// A mixin setter.
pub type MixinSet = Arc<dyn Fn(&Context, Arc<dyn Any + Send + Sync>) + Send + Sync>;

/// A callable-service invocation handler (`[Service.invoke]` in the TS
/// reference).
pub type InvokeFn = Arc<
    dyn Fn(
            &ShadowContext,
            Option<&Arc<dyn Any + Send + Sync>>,
        ) -> Option<Arc<dyn Any + Send + Sync>>
        + Send
        + Sync,
>;

/// A registered accessor (`Property.Accessor` in reflect.ts).
pub struct MixinAccessor {
    /// Resolves the value.
    pub get: MixinGet,
    /// Optionally writes the value.
    pub set: Option<MixinSet>,
}

/// The concrete store error reported by the `internal/set` waterfall tail:
/// without it, a rejected write would always surface the generic "without
/// provide" message instead of the real reason.
struct SetError(String);

/// A service label. Labels compare by value: contexts isolated with the same
/// label share the same service instance (mirrors `Symbol('name')` equality
/// in the TS reference).
pub type Label = Arc<str>;

/// One shared layer of the overlay chain: both the isolate labels and the
/// intercept overrides visible from a context.
pub(crate) struct OverlayLayer {
    state: ArcSwap<OverlayState>,
}

/// The mutable part of an overlay layer; replaced atomically on change.
#[derive(Clone, Debug, Default)]
pub(crate) struct OverlayState {
    pub(crate) isolate: HashMap<String, Label>,
    pub(crate) intercept: HashMap<String, Arc<dyn Any + Send + Sync>>,
    pub(crate) parent: Option<Arc<OverlayLayer>>,
}

impl Default for OverlayLayer {
    fn default() -> Self {
        Self {
            state: ArcSwap::from_pointee(OverlayState::default()),
        }
    }
}

impl std::fmt::Debug for OverlayLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OverlayLayer")
            .field("state", &self.state.load_full())
            .finish()
    }
}

impl OverlayLayer {
    /// Loads the current snapshot.
    pub(crate) fn load(&self) -> Arc<OverlayState> {
        self.state.load_full()
    }

    /// Creates a layer with the given entries and parent chain.
    pub(crate) fn with(
        isolate: HashMap<String, Label>,
        intercept: HashMap<String, Arc<dyn Any + Send + Sync>>,
        parent: Option<Arc<Self>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: ArcSwap::from_pointee(OverlayState {
                isolate,
                intercept,
                parent,
            }),
        })
    }

    /// Resolves the isolate label for `name` along the chain.
    fn lookup_isolate(&self, name: &str) -> Option<Label> {
        let state = self.state.load_full();
        if let Some(label) = state.isolate.get(name) {
            return Some(label.clone());
        }
        state.parent.as_ref()?.lookup_isolate(name)
    }

    /// Returns the bottom-most (root) layer of the chain.
    fn bottom(self: &Arc<Self>) -> Arc<Self> {
        let mut layer = self.clone();
        loop {
            let next = layer.state.load_full().parent.clone();
            match next {
                Some(parent) => layer = parent,
                None => return layer,
            }
        }
    }

    fn insert_isolate(&self, name: &str, label: Label) {
        let mut state = (*self.state.load_full()).clone();
        state.isolate.insert(name.to_string(), label);
        self.state.store(Arc::new(state));
    }

    fn remove_isolate(&self, name: &str) {
        let mut state = (*self.state.load_full()).clone();
        state.isolate.remove(name);
        self.state.store(Arc::new(state));
    }

    fn clear_isolate(&self) {
        let mut state = (*self.state.load_full()).clone();
        state.isolate.clear();
        self.state.store(Arc::new(state));
    }

    fn insert_intercept(&self, name: &str, config: Arc<dyn Any + Send + Sync>) {
        let mut state = (*self.state.load_full()).clone();
        state.intercept.insert(name.to_string(), config);
        self.state.store(Arc::new(state));
    }

    fn remove_intercept(&self, name: &str) {
        let mut state = (*self.state.load_full()).clone();
        state.intercept.remove(name);
        self.state.store(Arc::new(state));
    }

    fn clear_intercept(&self) {
        let mut state = (*self.state.load_full()).clone();
        state.intercept.clear();
        self.state.store(Arc::new(state));
    }

    /// Atomically replaces both maps of the top layer (single snapshot
    /// store: readers never observe a half-applied reconfiguration).
    fn replace(
        &self,
        isolate: HashMap<String, Label>,
        intercept: HashMap<String, Arc<dyn Any + Send + Sync>>,
    ) {
        let mut state = (*self.state.load_full()).clone();
        state.isolate = isolate;
        state.intercept = intercept;
        self.state.store(Arc::new(state));
    }

    /// Re-points the parent chain (mirrors `Object.setPrototypeOf`).
    fn set_parent(&self, parent: Option<Arc<Self>>) {
        let mut state = (*self.state.load_full()).clone();
        state.parent = parent;
        self.state.store(Arc::new(state));
    }
}

/// An entry of the shared service store.
#[derive(Clone)]
pub(crate) struct StoreEntry {
    pub name: String,
    pub value: Arc<dyn Any + Send + Sync>,
    pub fiber: Weak<Fiber>,
    /// The inner state of the context on which the service was provided
    /// (the JS `symbols.shadow`). Only the inner is kept: holding a full
    /// [`Context`] here would strongly pin the provider fiber and create an
    /// `Arc` cycle through `Fiber::resolved`.
    pub(crate) shadow_inner: Arc<ContextInner>,
    pub check: Option<ServiceCheck>,
    pub invoke: Option<InvokeFn>,
}

impl std::fmt::Debug for StoreEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreEntry")
            .field("name", &self.name)
            .field("fiber_uid", &self.fiber.upgrade().map(|f| f.uid()))
            .finish()
    }
}

/// The service store shared by a whole context chain.
#[derive(Clone, Debug, Default)]
pub(crate) struct Store {
    pub(crate) by_label: HashMap<Label, Arc<StoreEntry>>,
}

/// Shared inner state of a [`Context`].
pub(crate) struct ContextInner {
    pub overlay: Arc<OverlayLayer>,
    pub store: Arc<ArcSwap<Store>>,
    /// Serializes compound snapshot mutations (store / props / layer writes).
    /// Never held while dispatching events or running effects. Reads stay
    /// lock-free via `ArcSwap` / atomics.
    pub write_lock: Arc<Mutex<()>>,
    pub meta: Mutex<Vec<(String, Arc<dyn Any + Send + Sync>)>>,
    /// Shared accessor table for the whole context tree (mirrors the single
    /// `ReflectService.props` in the TS reference; accessors registered by
    /// any fiber are visible tree-wide).
    pub props: Arc<ArcSwap<HashMap<String, Arc<MixinAccessor>>>>,
}

impl std::fmt::Debug for ContextInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextInner")
            .field("overlay", &self.overlay)
            .field("store", &self.store.load_full())
            .field("meta", &self.meta.lock().unwrap())
            .finish_non_exhaustive()
    }
}

impl ContextInner {
    /// Resolves the isolate label for `name` along the chain.
    pub(crate) fn isolate_label(&self, name: &str) -> Option<Label> {
        self.overlay.lookup_isolate(name)
    }

    /// Strict store lookup: the entry must exist and its fiber must be
    /// `ACTIVE` (mirrors `_getImpl(name, true)` in reflect.ts).
    pub(crate) fn lookup_strict(&self, name: &str) -> Option<Arc<StoreEntry>> {
        let label = self.overlay.lookup_isolate(name)?;
        let entry = self.store.load_full().by_label.get(&label)?.clone();
        let active = entry
            .fiber
            .upgrade()
            .is_some_and(|fiber| fiber.state() == FiberState::Active);
        if active { Some(entry) } else { None }
    }

    /// Non-strict store lookup: the entry must exist, but the provider fiber
    /// need not be `ACTIVE`. Framework services (events/logger/reflect/
    /// registry) stay reachable even while their fiber is unloading, mirroring
    /// the TS prototype properties.
    pub(crate) fn lookup_non_strict(&self, name: &str) -> Option<Arc<StoreEntry>> {
        let label = self.overlay.lookup_isolate(name)?;
        self.store.load_full().by_label.get(&label).cloned()
    }

    /// Typed service lookup by name.
    pub(crate) fn get_service<S: Service>(&self, name: &str) -> Option<Arc<S>> {
        let entry = self.lookup_strict(name)?;
        entry.value.clone().downcast::<S>().ok()
    }

    /// Non-strict typed lookup (see [`ContextInner::lookup_non_strict`]).
    pub(crate) fn get_service_non_strict<S: Service>(&self, name: &str) -> Option<Arc<S>> {
        let entry = self.lookup_non_strict(name)?;
        entry.value.clone().downcast::<S>().ok()
    }
}

/// The recorded shadow of a registered service (mirrors the JS
/// `symbols.shadow` symbol).
///
/// In the TS reference the context a service belongs to is injected
/// implicitly through a Proxy (`this.ctx[symbols.shadow]`). The Rust port
/// passes contexts explicitly instead, and records the shadow here so the
/// information stays queryable without hidden state: the
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
    pub fiber: Weak<Fiber>,
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
    pub fn new(own: Context, caller: Context) -> Self {
        Self { own, caller }
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
    pub fn for_service(&self, next_own: Context) -> Self {
        Self {
            own: next_own,
            caller: self.caller.clone(),
        }
    }

    /// Dynamic dependency read through the service's own scope (mirrors
    /// `this.ctx[name]` in the TS reference).
    pub fn get_str(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.own.get_str(name)
    }

    /// Strict dynamic dependency read through the service's own scope.
    pub fn get_str_strict(&self, name: &str) -> Result<Arc<dyn Any + Send + Sync>, String> {
        self.own.get_str_strict(name)
    }

    /// Non-strict dynamic dependency read through the service's own scope.
    pub fn get_str_non_strict(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.own.get_str_non_strict(name)
    }

    /// Typed dependency read through the service's own scope.
    pub fn get<S: Service>(&self) -> Option<Arc<S>> {
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
    pub fn invoke_str(
        &self,
        name: &str,
        init: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        let entry = self.own.inner.lookup_strict(name)?;
        let invoke = entry.invoke.as_ref()?;
        let own = Context {
            inner: entry.shadow_inner.clone(),
            fiber: entry.fiber.upgrade()?,
        };
        invoke(
            &Self {
                own,
                caller: self.caller.clone(),
            },
            init.as_ref(),
        )
    }

    /// Typed variant of [`ShadowContext::invoke_str`].
    pub fn invoke<S: Service>(
        &self,
        init: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
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
/// A context is `Send + Sync`: shared state lives behind `Arc` with
/// lock-free snapshots (`ArcSwap`), atomics and short-scoped `Mutex`es, so
/// plugin tasks can run on worker threads.
#[derive(Clone)]
pub struct Context {
    pub(crate) inner: Arc<ContextInner>,
    pub(crate) fiber: Arc<Fiber>,
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
    pub fn new() -> Self {
        let overlay = Arc::new(OverlayLayer::default());
        let store = Arc::new(ArcSwap::from_pointee(Store::default()));
        let inner = Arc::new(ContextInner {
            overlay,
            store,
            write_lock: Arc::new(Mutex::new(())),
            meta: Mutex::new(Vec::new()),
            props: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        });
        let fiber = Fiber::root(inner.clone());
        let ctx = Self { inner, fiber };

        // Framework services are visible on every context (context.ts).
        ctx.provide_inner(EventsService::default());
        ctx.provide_inner(LoggerService::default());
        ctx.provide_inner(ReflectService);
        ctx.provide_inner(RegistryService::default());
        // context.ts clears the root fiber's disposables after framework
        // services are registered, so they don't surface as user effects.
        ctx.fiber.disposables.lock().unwrap().clear();
        ctx
    }

    /// Returns the fiber associated with this context.
    pub fn fiber(&self) -> &Arc<Fiber> {
        &self.fiber
    }

    /// Whether both contexts share the same inner state segment (used by the
    /// loader to identify an entry's own context).
    pub fn shares_inner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Resolves the isolate label for `name` along this context's chain.
    pub fn isolate_label(&self, name: &str) -> Option<Label> {
        self.inner.isolate_label(name)
    }

    /// Returns a context sharing this context's state but bound to `fiber`.
    pub fn with_fiber(&self, fiber: Arc<Fiber>) -> Self {
        Self {
            inner: self.inner.clone(),
            fiber,
        }
    }

    /// Looks up a service by name.
    ///
    /// Returns `None` when no active provider is visible from this context
    /// (it never panics on a missing entry).
    ///
    /// Dynamic access runs through the `internal/get` waterfall: listeners
    /// may override the value or call `next()` to fall back to the strict
    /// store lookup.
    pub fn get_str(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let error = format!("cannot get property \"{name}\" without inject");
        let args: Vec<Arc<dyn Any + Send + Sync>> = vec![
            Arc::new(self.clone()),
            Arc::new(name.to_string()),
            Arc::new(error),
        ];
        let this = self.clone();
        let name = name.to_string();
        let tail: WaterfallNext = Arc::new(move || {
            let this = this.clone();
            let name = name.clone();
            Box::pin(async move {
                Ok(this
                    .inner
                    .lookup_strict(&name)
                    .map(|entry| entry.value.clone()))
            })
        });
        let mut future = self.events().expect("events service").waterfall(
            self,
            "internal/get",
            &args,
            None,
            tail,
        );
        match poll_once(&mut future) {
            Poll::Ready(Ok(result)) => result,
            Poll::Ready(Err(_)) => None,
            Poll::Pending => {
                self.logger()
                    .warn("internal/get waterfall did not complete synchronously");
                None
            }
        }
    }

    /// Whether any visible store entry with `name` exists (used by the
    /// loader to detect provider-side re-scoping).
    pub fn provides(&self, name: &str) -> bool {
        self.inner
            .store
            .load_full()
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
    /// service methods receive contexts explicitly — so this query exists to
    /// keep the information observable and auditable.
    ///
    /// The lookup is non-strict: the shadow remains available as long as the
    /// entry exists, even while the provider fiber is unloading or failed
    /// (mirrors the service object keeping its `ctx` in JS).
    pub fn shadow_of(&self, name: &str) -> Option<ServiceShadow> {
        let entry = self.inner.lookup_non_strict(name)?;
        let fiber = entry.fiber.upgrade()?;
        Some(ServiceShadow {
            name: entry.name.clone(),
            ctx: Self {
                inner: entry.shadow_inner.clone(),
                fiber,
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
        provider: &Arc<Fiber>,
    ) -> bool {
        let _guard = self.inner.write_lock.lock().unwrap();
        let mut store = (*self.inner.store.load_full()).clone();
        let Some(entry) = store.by_label.get(old_label).cloned() else {
            return false;
        };
        if entry.name != name {
            return false;
        }
        let owned_by = entry
            .fiber
            .upgrade()
            .is_some_and(|fiber| Arc::ptr_eq(&fiber, provider));
        if !owned_by || store.by_label.contains_key(new_label) {
            return false;
        }
        store.by_label.remove(old_label);
        store.by_label.insert(new_label.clone(), entry);
        self.inner.store.store(Arc::new(store));
        true
    }

    /// Moves a store entry from `old_label` to `new_label` when the provider
    /// itself now resolves `new_label` (mirrors the TS isolate patch-context
    /// delimiter guard: the impl's fiber realm moved with it).
    pub fn migrate_label(&self, name: &str, old_label: &Label, new_label: &Label) -> bool {
        // Lock-order discipline: resolve the isolate label before taking the
        // write lock, so no path holds the store lock while touching isolate.
        let provider_moved = self
            .inner
            .store
            .load_full()
            .by_label
            .get(old_label)
            .cloned()
            .is_some_and(|entry| {
                entry.name == name
                    && entry.fiber.upgrade().is_some_and(|fiber| {
                        fiber
                            .ctx
                            .isolate_label(name)
                            .is_some_and(|label| &label == new_label)
                    })
            });
        if !provider_moved {
            return false;
        }
        let _guard = self.inner.write_lock.lock().unwrap();
        let mut store = (*self.inner.store.load_full()).clone();
        let Some(entry) = store.by_label.remove(old_label) else {
            return false;
        };
        if store.by_label.contains_key(new_label) {
            store.by_label.insert(old_label.clone(), entry);
            return false;
        }
        store.by_label.insert(new_label.clone(), entry);
        self.inner.store.store(Arc::new(store));
        true
    }

    /// Strict dynamic access (mirrors the throwing `ctx[name]` access in the
    /// TS reference).
    ///
    /// Returns an error with the TS-compatible message when the property is
    /// missing or the context is inactive.
    pub fn get_str_strict(&self, name: &str) -> Result<Arc<dyn Any + Send + Sync>, String> {
        if self.fiber.uid().is_none() {
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
    pub fn get_str_non_strict(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
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
        self.inner.props.load_full().contains_key(name)
            || self.inner.lookup_non_strict(name).is_some()
    }

    /// Dynamic set (mirrors `ctx[name] = value` in the TS reference).
    ///
    /// Dynamic writes run through the `internal/set` waterfall: listeners
    /// may accept the write (bail with `true`) or call `next()` to fall back
    /// to the strict store update.
    ///
    /// The strict store update enforces ownership: only the fiber that
    /// provided the service may set its value; otherwise the
    /// `"cannot set property \"{name}\" in multiple fibers"` error is
    /// returned.
    pub fn set_str(&self, name: &str, value: Arc<dyn Any + Send + Sync>) -> Result<(), String> {
        let error = format!("cannot set property \"{name}\" without provide");
        let args: Vec<Arc<dyn Any + Send + Sync>> = vec![
            Arc::new(self.clone()),
            Arc::new(name.to_string()),
            value.clone(),
            Arc::new(error.clone()),
        ];
        let this = self.clone();
        let name = name.to_string();
        let tail: WaterfallNext = Arc::new(move || {
            let this = this.clone();
            let name = name.clone();
            let value = value.clone();
            Box::pin(async move {
                let result: Option<Arc<dyn Any + Send + Sync>> =
                    match this.set_str_impl(&name, value) {
                        Ok(()) => Some(Arc::new(true)),
                        Err(message) => Some(Arc::new(SetError(message))),
                    };
                Ok(result)
            })
        });
        let mut future = self.events().expect("events service").waterfall(
            self,
            "internal/set",
            &args,
            None,
            tail,
        );
        match poll_once(&mut future) {
            Poll::Ready(Ok(Some(result)))
                if result.downcast_ref::<bool>().copied().unwrap_or(false) =>
            {
                Ok(())
            }
            Poll::Ready(Ok(Some(result))) => Err(result
                .downcast_ref::<SetError>()
                .map(|set_error| set_error.0.clone())
                .unwrap_or(error)),
            Poll::Ready(Ok(None)) | Poll::Ready(Err(_)) => Err(error),
            Poll::Pending => {
                self.logger()
                    .warn("internal/set waterfall did not complete synchronously");
                Err(error)
            }
        }
    }

    fn set_str_impl(&self, name: &str, value: Arc<dyn Any + Send + Sync>) -> Result<(), String> {
        let label = self
            .inner
            .overlay
            .lookup_isolate(name)
            .ok_or_else(|| format!("cannot set property \"{name}\" without provide"))?;
        let _guard = self.inner.write_lock.lock().unwrap();
        let mut store = (*self.inner.store.load_full()).clone();
        match store.by_label.get(&label).cloned() {
            Some(entry) => {
                // Ownership: only the providing fiber may update the value
                // (mirrors reflect.set "cannot set property in multiple
                // fibers").
                let owned = entry
                    .fiber
                    .upgrade()
                    .is_some_and(|fiber| Arc::ptr_eq(&fiber, &self.fiber));
                if !owned {
                    return Err(format!("cannot set property \"{name}\" in multiple fibers"));
                }
                let name = entry.name.clone();
                let fiber = entry.fiber.clone();
                let shadow_inner = entry.shadow_inner.clone();
                let check = entry.check.clone();
                let invoke = entry.invoke.clone();
                store.by_label.insert(
                    label,
                    Arc::new(StoreEntry {
                        name,
                        value,
                        fiber,
                        shadow_inner,
                        check,
                        invoke,
                    }),
                );
            }
            None => return Err(format!("cannot set property \"{name}\" without provide")),
        }
        self.inner.store.store(Arc::new(store));
        drop(_guard);
        // Notify injectors that the service value changed.
        drop(self.notify(name));
        Ok(())
    }

    /// Whether the value is a [`Context`] (mirrors `Context.is(value)`).
    pub fn is_context(value: &dyn Any) -> bool {
        value.is::<Self>()
    }

    /// Looks up a typed service.
    pub fn get<S: Service>(&self) -> Option<Arc<S>> {
        self.get_str(S::NAME)?.downcast::<S>().ok()
    }

    /// Registers a service by name through the current fiber's effect system.
    ///
    /// The value is stored under the isolate label resolved for `name`; when
    /// no label exists yet, one is created on the root layer (mirroring
    /// `ctx.root[symbols.isolate][name] ??= Symbol(name)` in the TS source).
    /// The returned handle disposes the registration and notifies consumers.
    pub fn provide_str(
        &self,
        name: &str,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Result<Arc<EffectHandle>, String> {
        self.provide_str_with_check(name, value, None)
    }

    /// Registers a service with an explicit availability check.
    pub fn provide_str_with_check(
        &self,
        name: &str,
        value: Arc<dyn Any + Send + Sync>,
        check: Option<ServiceCheck>,
    ) -> Result<Arc<EffectHandle>, String> {
        self.provide_inner_impl(name, value, check, None)
    }

    fn provide_inner_impl(
        &self,
        name: &str,
        value: Arc<dyn Any + Send + Sync>,
        check: Option<ServiceCheck>,
        invoke: Option<InvokeFn>,
    ) -> Result<Arc<EffectHandle>, String> {
        self.fiber.assert_active().map_err(|e| e.message)?;
        {
            let props = self.inner.props.load_full();
            if props.contains_key(name) {
                return Err(format!("property \"{name}\" is already declared"));
            }
        }
        let label = self.ensure_label(name);
        {
            let store = self.inner.store.load_full();
            if let Some(existing) = store.by_label.get(&label) {
                return Err(format!(
                    "service \"{}\" has been registered at <{}>",
                    existing.name,
                    existing
                        .fiber
                        .upgrade()
                        .map_or_else(|| "?".to_string(), |f| f.name())
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
                    let entry = Arc::new(StoreEntry {
                        name: name.clone(),
                        value,
                        fiber: Arc::downgrade(&ctx.fiber),
                        shadow_inner: ctx.inner.clone(),
                        check: check.clone(),
                        invoke: invoke.clone(),
                    });
                    let _guard = ctx.inner.write_lock.lock().unwrap();
                    let mut store = (*ctx.inner.store.load_full()).clone();
                    // Re-check under the write lock: a concurrent registration
                    // may have won the race since the pre-check above.
                    if store.by_label.contains_key(&label) {
                        drop(_guard);
                        return Effect::Error(
                            format!("service \"{name}\" is already registered").into(),
                        );
                    }
                    store.by_label.insert(label.clone(), entry.clone());
                    ctx.inner.store.store(Arc::new(store));
                    drop(_guard);
                    ctx.fiber
                        .resolved
                        .lock()
                        .unwrap()
                        .insert(name.clone(), entry);
                    if ctx.fiber.state() == FiberState::Active {
                        ctx.notify(&name);
                    }
                    let ctx = ctx.clone();
                    Effect::Disposer(Box::new(move || {
                        let ctx = ctx.clone();
                        let name = name.clone();
                        let label = label;
                        Box::pin(async move {
                            let _guard = ctx.inner.write_lock.lock().unwrap();
                            let mut store = (*ctx.inner.store.load_full()).clone();
                            store.by_label.remove(&label);
                            ctx.inner.store.store(Arc::new(store));
                            drop(_guard);
                            let fibers = ctx.notify(&name);
                            // The TS reference awaits the affected fibers
                            // here; the current implementation keeps the
                            // removal and notify synchronous, which matches
                            // the fiber spec expectations under fake timers.
                            let _ = fibers;
                            ctx.fiber.resolved.lock().unwrap().remove(&name);
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
    pub fn provide<S: Service>(&self, value: Arc<S>) -> Result<Arc<EffectHandle>, String> {
        let check = {
            let value = value.clone();
            Arc::new(move |ctx: &Self| value.check(ctx)) as ServiceCheck
        };
        let invoke = {
            let value = value.clone();
            Arc::new(
                move |ctx: &ShadowContext, init: Option<&Arc<dyn Any + Send + Sync>>| {
                    value.invoke(ctx, init)
                },
            ) as InvokeFn
        };
        self.provide_inner_impl(S::NAME, value, Some(check), Some(invoke))
    }

    /// Invokes a callable service (mirrors `ctx[service](...)`).
    ///
    /// The invocation receives a [`ShadowContext`] whose `own` is the
    /// callable's recorded shadow (its own registration context) and whose
    /// caller is this context — the faithful counterpart of the JS proxy
    /// injecting `this.ctx` into `[Service.invoke]`.
    pub fn invoke_str(
        &self,
        name: &str,
        init: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        let entry = self.inner.lookup_strict(name)?;
        let invoke = entry.invoke.as_ref()?;
        let own = Self {
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
    pub fn invoke<S: Service>(
        &self,
        init: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        self.invoke_str(S::NAME, init)
    }

    /// Returns a new context with an additional isolate layer.
    ///
    /// The child shares the store and fiber of its parent, but maps `name` to
    /// `label`. Services provided by the child are invisible to the parent,
    /// while services already visible to the parent remain visible to the
    /// child unless the child isolates the same name with a different label.
    pub fn isolate(&self, name: &str, label: Label) -> Self {
        let layer = OverlayLayer::with(
            HashMap::from([(name.to_string(), label)]),
            HashMap::new(),
            Some(self.inner.overlay.clone()),
        );
        self.spawn(layer)
    }

    /// Returns a context with an empty isolate layer on top (used by the
    /// loader to scope entry services per-realm).
    pub fn with_isolate_layer(&self) -> Self {
        let layer = OverlayLayer::with(
            HashMap::new(),
            HashMap::new(),
            Some(self.inner.overlay.clone()),
        );
        self.spawn(layer)
    }

    /// Sets an isolate label on this context's top layer.
    pub fn set_isolate(&self, name: &str, label: Label) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner.overlay.insert_isolate(name, label);
    }

    /// Removes an isolate label from this context's top layer.
    pub fn remove_isolate(&self, name: &str) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner.overlay.remove_isolate(name);
    }

    /// Clears all entries on this context's top isolate layer.
    pub fn clear_isolate_layer(&self) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner.overlay.clear_isolate();
    }

    /// Returns a context with an empty intercept layer on top.
    pub fn with_intercept_layer(&self) -> Self {
        let layer = OverlayLayer::with(
            HashMap::new(),
            HashMap::new(),
            Some(self.inner.overlay.clone()),
        );
        self.spawn(layer)
    }

    /// Sets an intercept config on this context's top layer.
    pub fn set_intercept(&self, name: &str, config: Arc<dyn Any + Send + Sync>) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner.overlay.insert_intercept(name, config);
    }

    /// Removes an intercept config from this context's top layer.
    pub fn remove_intercept(&self, name: &str) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner.overlay.remove_intercept(name);
    }

    /// Clears all entries on this context's top intercept layer.
    pub fn clear_intercept_layer(&self) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner.overlay.clear_intercept();
    }

    /// Returns a new context with an additional intercept layer.
    ///
    /// Config entries registered here override entries from parent layers
    /// when [`Context::resolve_config`] merges the chain.
    pub fn intercept<C: Config + Send + Sync>(&self, name: &str, config: C) -> Self {
        let layer = OverlayLayer::with(
            HashMap::new(),
            HashMap::from([(
                name.to_string(),
                Arc::new(config) as Arc<dyn Any + Send + Sync>,
            )]),
            Some(self.inner.overlay.clone()),
        );
        self.spawn(layer)
    }

    /// Returns a new context carrying arbitrary metadata.
    ///
    /// Metadata entries are appended to those of the parent; lookups prefer
    /// the nearest entry with the same key.
    pub fn extend(&self, meta: &[(&str, Arc<dyn Any + Send + Sync>)]) -> Self {
        let ctx = self.spawn(self.inner.overlay.clone());
        let mut entries = self.inner.meta.lock().unwrap().clone();
        for (key, value) in meta {
            entries.push((key.to_string(), value.clone()));
        }
        *ctx.inner.meta.lock().unwrap() = entries;
        ctx
    }

    /// Returns a metadata value previously attached via [`Context::extend`].
    pub fn meta<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        for (k, value) in self.inner.meta.lock().unwrap().iter().rev() {
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
        let mut configs: Vec<Arc<dyn Any + Send + Sync>> = Vec::new();
        let mut layer = Some(self.inner.overlay.clone());
        while let Some(current) = layer {
            let state = current.load();
            if let Some(config) = state.intercept.get(name) {
                configs.push(config.clone());
            }
            layer = state.parent.clone();
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
    pub fn intercept_chain(&self, name: &str) -> Vec<Arc<dyn Any + Send + Sync>> {
        let mut result = Vec::new();
        let mut layer = Some(self.inner.overlay.clone());
        while let Some(current) = layer {
            let state = current.load();
            if let Some(config) = state.intercept.get(name) {
                result.push(config.clone());
            }
            layer = state.parent.clone();
        }
        result
    }

    /// Atomically replaces the top overlay layer: both the isolate labels and
    /// the intercept overrides are published in a single snapshot store, so
    /// concurrent readers never observe a half-applied reconfiguration
    /// (used by the loader when an entry's overlay options change).
    pub fn apply_overlay(
        &self,
        isolate: &HashMap<String, Label>,
        intercept: &HashMap<String, Arc<dyn Any + Send + Sync>>,
    ) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner
            .overlay
            .replace(isolate.clone(), intercept.clone());
    }

    /// Re-points the top overlay layer at `new_parent`'s chain (mirrors
    /// `Object.setPrototypeOf(ctx, parent.ctx)` when an entry moves between
    /// groups; the fiber keeps running).
    pub fn reparent(&self, new_parent: &Self) {
        let _guard = self.inner.write_lock.lock().unwrap();
        self.inner
            .overlay
            .set_parent(Some(new_parent.inner.overlay.clone()));
    }

    /// Registers mixin accessors (mirrors `ctx.mixin(source, map)`).
    ///
    /// Each `(key, target_name)` entry makes `ctx.resolve_assoc(source, key)`
    /// resolve the target service or value. The registration is an effect of
    /// the current fiber and is removed when the fiber unloads.
    pub fn mixin(&self, source: &str, entries: &[(&str, &str)]) -> Result<(), String> {
        let accessors: Vec<(String, Arc<MixinAccessor>)> = entries
            .iter()
            .map(|(key, target_name)| {
                let target_name = target_name.to_string();
                let get: MixinGet = Arc::new(move |ctx| ctx.get_str(&target_name));
                (key.to_string(), Arc::new(MixinAccessor { get, set: None }))
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
                (key.to_string(), Arc::new(MixinAccessor { get, set }))
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
    ) -> Result<Arc<EffectHandle>, String> {
        let this = self.clone();
        let name = name.to_string();
        let label = format!("ctx.accessor({name:?})");
        self.fiber
            .effect(
                move || {
                    // Conflicts with an existing accessor or a same-name
                    // service are rejected (mirrors the TS accessor effect).
                    let exists = this.inner.lookup_non_strict(&name).is_some()
                        || this.inner.props.load_full().contains_key(&name);
                    if exists {
                        return Effect::Error(
                            format!("property \"{name}\" is already declared").into(),
                        );
                    }
                    let _guard = this.inner.write_lock.lock().unwrap();
                    let mut props = (*this.inner.props.load_full()).clone();
                    if props.contains_key(&name) {
                        drop(_guard);
                        return Effect::Error(
                            format!("property \"{name}\" is already declared").into(),
                        );
                    }
                    props.insert(name.clone(), Arc::new(MixinAccessor { get, set }));
                    this.inner.props.store(Arc::new(props));
                    drop(_guard);
                    let this_for_dispose = this.clone();
                    let name_for_dispose = name;
                    Effect::Disposer(sync_disposer(move || {
                        let _guard = this_for_dispose.inner.write_lock.lock().unwrap();
                        let mut props = (*this_for_dispose.inner.props.load_full()).clone();
                        props.remove(&name_for_dispose);
                        this_for_dispose.inner.props.store(Arc::new(props));
                    }))
                },
                &label,
            )
            .map_err(|error| error.message)
    }

    fn register_accessors(
        &self,
        source: &str,
        entries: Vec<(String, Arc<MixinAccessor>)>,
    ) -> Result<(), String> {
        let this = self.clone();
        let source = source.to_string();
        let keys: Vec<String> = entries.iter().map(|(key, _)| key.clone()).collect();
        let label = format!("ctx.mixin({source:?})");
        self.fiber
            .effect(
                move || {
                    let conflict = {
                        let _guard = this.inner.write_lock.lock().unwrap();
                        let mut props = (*this.inner.props.load_full()).clone();
                        let mut conflict = None;
                        for (key, accessor) in entries {
                            let full = format!("{source}.{key}");
                            if props.contains_key(&key) {
                                conflict = Some(format!("property \"{full}\" is already declared"));
                                break;
                            }
                            props.insert(key, accessor);
                        }
                        if conflict.is_none() {
                            this.inner.props.store(Arc::new(props));
                        }
                        conflict
                    };
                    if let Some(message) = conflict {
                        return Effect::Error(message.into());
                    }
                    let this_for_dispose = this.clone();
                    Effect::Disposer(sync_disposer(move || {
                        let _guard = this_for_dispose.inner.write_lock.lock().unwrap();
                        let mut props = (*this_for_dispose.inner.props.load_full()).clone();
                        for key in keys {
                            props.remove(&key);
                        }
                        this_for_dispose.inner.props.store(Arc::new(props));
                    }))
                },
                &label,
            )
            .map(|_handle| ())
            .map_err(|error| error.message)
    }

    /// Resolves an associated value `source.key` (mirrors the property
    /// access `ctx[source].key` in the TS reference).
    pub fn resolve_assoc(&self, source: &str, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        let _ = source;
        let props = self.inner.props.load_full();
        let accessor = props.get(key)?.clone();
        (accessor.get)(self)
    }

    /// Sets an associated value `source.key`.
    pub fn set_assoc(
        &self,
        source: &str,
        key: &str,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), String> {
        let full = format!("{source}.{key}");
        let accessor = {
            let props = self.inner.props.load_full();
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
    pub fn plugin(
        &self,
        plugin: &Plugin,
        config: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Arc<Fiber> {
        self.plugin_with_validator(plugin, config, None)
    }

    /// Registers a plugin via the registry service (used by the loader).
    pub fn registry_plugin(
        &self,
        plugin: &Plugin,
        config: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Arc<Fiber> {
        let registry = self
            .get::<RegistryService>()
            .expect("registry service must be present");
        registry.plugin(self, plugin, config)
    }

    /// Registers a plugin with a config validator.
    pub fn plugin_with_validator(
        &self,
        plugin: &Plugin,
        config: Option<Arc<dyn Any + Send + Sync>>,
        validator: Option<ConfigValidator>,
    ) -> Arc<Fiber> {
        let registry = self
            .get::<RegistryService>()
            .expect("registry service must be present");
        registry.plugin_with_validator(self, plugin, config, validator)
    }

    /// Registers an inject callback (a plugin whose only role is consuming
    /// the declared dependencies).
    pub fn inject(&self, deps: &[&str], callback: ApplyFn) -> Arc<Fiber> {
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
    ) -> Result<Arc<EffectHandle>, crate::fiber::CordisError>
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
    ) -> Result<Arc<EffectHandle>, CordisError> {
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
    fn events(&self) -> Option<Arc<EventsService>> {
        self.inner.get_service_non_strict::<EventsService>("events")
    }

    /// Registers a listener with an attached filter.
    pub fn on_filtered(
        &self,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
        filter: crate::events::ListenerFilter,
    ) -> Result<Arc<EffectHandle>, CordisError> {
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
    ) -> Result<Arc<EffectHandle>, CordisError> {
        self.events()
            .expect("events service")
            .once(self, event, callback, options)
    }

    /// Emits an event (mirrors `ctx.emit`).
    pub fn emit(&self, event: &str, args: &[Arc<dyn Any + Send + Sync>]) {
        self.events()
            .expect("events service")
            .emit(self, event, args);
    }

    /// Emits with a filter (mirrors `ctx.emit(thisArg, name, ...)`).
    pub fn emit_with(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        this_arg: &dyn EventFilter,
    ) {
        self.events()
            .expect("events service")
            .emit_with(self, event, args, Some(this_arg));
    }

    /// Resolves a listener snapshot without invoking the listeners (mirrors
    /// the JS `_resolve` split used by publication boundaries that must
    /// capture the snapshot before committing a mutation).
    pub fn resolve_emit(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Vec<EventCallback> {
        self.events()
            .expect("events service")
            .resolve_callbacks(self, event, args, this_arg)
    }

    /// Invokes a previously resolved listener snapshot with per-listener
    /// containment: failures are logged, never propagated.
    pub fn emit_resolved_contained(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        callbacks: Vec<EventCallback>,
    ) {
        self.events()
            .expect("events service")
            .emit_resolved_contained(self, event, callbacks, args);
    }

    /// Invokes a previously resolved listener snapshot with veto semantics:
    /// the first synchronous failure propagates; asynchronous rejections are
    /// logged and cannot veto the synchronous boundary.
    pub fn emit_resolved_veto(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        callbacks: Vec<EventCallback>,
    ) -> Result<(), BoxError> {
        self.events()
            .expect("events service")
            .emit_resolved_veto(self, event, callbacks, args)
    }

    /// Runs listeners concurrently (mirrors `ctx.parallel`).
    pub async fn parallel(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<(), ParallelError> {
        self.events()
            .expect("events service")
            .parallel(self, event, args, this_arg)
            .await
    }

    /// Awaits an already-resolved listener snapshot together (mirrors the
    /// JS split between `_resolve` and a later `parallel` dispatch).
    pub async fn parallel_resolved(
        &self,
        event: &str,
        callbacks: Vec<EventCallback>,
        args: &[Arc<dyn Any + Send + Sync>],
    ) -> Result<(), ParallelError> {
        self.events()
            .expect("events service")
            .parallel_resolved(event, callbacks, args)
            .await
    }

    /// Runs listeners sequentially (mirrors `ctx.serial`).
    pub async fn serial(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Arc<dyn Any + Send + Sync>>, BoxError> {
        self.events()
            .expect("events service")
            .serial(self, event, args, this_arg)
            .await
    }

    /// Runs listeners synchronously with bail semantics (mirrors `ctx.bail`).
    pub fn bail(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Arc<dyn Any + Send + Sync>>, BoxError> {
        self.events()
            .expect("events service")
            .bail(self, event, args, this_arg)
    }

    /// Runs listeners in a waterfall chain (mirrors `ctx.waterfall`).
    ///
    /// The chain is awaited as a whole; async listeners may call the `next`
    /// function and await it (mirrors the JS waterfall, where async
    /// listeners make the whole chain awaitable). An optional `this_arg`
    /// filters listeners by scope (mirrors the JS `thisArg`).
    pub async fn waterfall(
        &self,
        event: &str,
        args: &[Arc<dyn Any + Send + Sync>],
        this_arg: Option<&dyn EventFilter>,
        tail: crate::events::WaterfallNext,
    ) -> Result<Option<Arc<dyn Any + Send + Sync>>, BoxError> {
        self.events()
            .expect("events service")
            .waterfall(self, event, args, this_arg, tail)
            .await
    }

    /// Notifies fibers that depend on `name` (mirrors `ReflectService.notify`).
    pub fn notify(&self, name: &str) -> Vec<Arc<Fiber>> {
        let Some(registry) = self.get::<RegistryService>() else {
            return Vec::new();
        };
        registry.notify(name, self)
    }

    /// Notifies fibers whose isolate label for `name` matches one of
    /// `labels` (used by the loader's realm migration).
    pub fn notify_with_labels(&self, name: &str, labels: &[Label]) -> Vec<Arc<Fiber>> {
        let Some(registry) = self.get::<RegistryService>() else {
            return Vec::new();
        };
        registry.notify_with_labels(name, labels)
    }

    fn spawn(&self, overlay: Arc<OverlayLayer>) -> Self {
        Self {
            inner: Arc::new(ContextInner {
                overlay,
                store: self.inner.store.clone(),
                write_lock: self.inner.write_lock.clone(),
                meta: Mutex::new(self.inner.meta.lock().unwrap().clone()),
                props: self.inner.props.clone(),
            }),
            fiber: self.fiber.clone(),
        }
    }

    fn ensure_label(&self, name: &str) -> Label {
        let _guard = self.inner.write_lock.lock().unwrap();
        if let Some(label) = self.inner.overlay.lookup_isolate(name) {
            return label;
        }
        let id = NEXT_LABEL_ID.fetch_add(1, Ordering::Relaxed);
        let label: Label = Arc::from(format!("{name}#{id}"));
        self.inner
            .overlay
            .bottom()
            .insert_isolate(name, label.clone());
        label
    }

    fn provide_inner<S: Service>(&self, value: S) {
        // Framework services never collide and live for the whole chain.
        drop(
            self.provide_str(S::NAME, Arc::new(value))
                .expect("framework service must register"),
        );
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}
