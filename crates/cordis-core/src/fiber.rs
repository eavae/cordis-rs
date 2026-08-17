//! Fiber lifecycle state machine and effect executor.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::Poll;

use crate::context::{Context, ContextInner, StoreEntry};
use crate::error::ConfigValidator;
use crate::events::{EventCallback, WaterfallNext, run_waterfall_step};
use crate::registry::Runtime;
use crate::service::{ApplyFn, BoxFuture, Disposer, Effect, EffectItem, sync_disposer};

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

/// Inject-resolution epoch of a fiber (mirrors the string epoch in fiber.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Epoch {
    /// At least one injected service is unavailable.
    Inactive,
    /// All injected services are resolved; the payload mirrors the TS
    /// `':'`-joined fiber uids.
    Active(String),
}

/// An error thrown by Cordis runtime invariants (mirrors `CordisError`).
#[derive(Debug, Clone)]
pub struct CordisError {
    /// Stable error code, e.g. `INACTIVE_EFFECT`.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
}

impl CordisError {
    /// Error raised when creating an effect on a disposed context.
    pub fn inactive_effect() -> Self {
        CordisError {
            code: "INACTIVE_EFFECT",
            message: "cannot create effect on inactive context".to_string(),
        }
    }
}

impl fmt::Display for CordisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for CordisError {}

/// Error propagated by [`Fiber::wait`] when the plugin entry failed.
#[derive(Debug, Clone)]
pub struct FiberError {
    message: String,
}

impl FiberError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        FiberError {
            message: message.into(),
        }
    }
}

impl fmt::Display for FiberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for FiberError {}

impl From<CordisError> for FiberError {
    fn from(error: CordisError) -> Self {
        FiberError::new(error.message)
    }
}

/// Effect metadata tree entry (mirrors `EffectMeta` in fiber.ts).
#[derive(Clone, Debug, PartialEq)]
pub struct EffectMeta {
    /// The label passed to [`Fiber::effect`].
    pub label: String,
    /// Nested effects collected during this effect's execution.
    pub children: Vec<EffectMeta>,
}

/// An entry of a fiber's (or an effect's) disposables list.
pub(crate) enum Disposable {
    /// A plain disposer (from a plugin apply or a yielded disposer).
    Direct(Disposer),
    /// A nested effect handle.
    Effect(Rc<EffectHandle>),
}

/// A fiber-level internal hook (e.g. `internal/update`).
#[derive(Clone)]
pub(crate) struct InternalHook {
    pub callback: EventCallback,
}

/// The optional background task produced by an async effect or apply.
pub(crate) type EffectTask = Option<BoxFuture<'static, Result<(), Box<dyn Error>>>>;

/// Idempotent effect handle returned by [`Fiber::effect`].
pub struct EffectHandle {
    /// Label used for diagnostics and effect metadata.
    pub label: String,
    epoch: Cell<bool>,
    disposables: RefCell<Vec<Disposable>>,
    has_task: Cell<bool>,
    task_done: Rc<Cell<bool>>,
    task_result: Rc<RefCell<Option<Result<(), String>>>>,
    meta: RefCell<EffectMeta>,
}

impl EffectHandle {
    fn new(label: &str) -> Rc<Self> {
        Rc::new(EffectHandle {
            label: label.to_string(),
            epoch: Cell::new(true),
            disposables: RefCell::new(Vec::new()),
            has_task: Cell::new(false),
            task_done: Rc::new(Cell::new(true)),
            task_result: Rc::new(RefCell::new(None)),
            meta: RefCell::new(EffectMeta {
                label: label.to_string(),
                children: Vec::new(),
            }),
        })
    }

    fn collect(&self, item: Disposable) {
        self.disposables.borrow_mut().push(item);
    }

    /// Whether the effect has already been disposed.
    pub fn is_disposed(&self) -> bool {
        !self.epoch.get()
    }

    /// The metadata tree of this effect.
    pub fn meta(&self) -> EffectMeta {
        self.meta.borrow().clone()
    }

    /// Runs the disposer chain (idempotent) and resolves with the first
    /// error, if any. Async effects are awaited before the chain runs.
    pub fn dispose(self: &Rc<Self>) -> BoxFuture<'static, Result<(), Box<dyn Error>>> {
        if !self.epoch.replace(false) {
            return Box::pin(async { Ok(()) });
        }
        if !self.has_task.get() {
            return Box::pin(self.clone().run_dispose_chain());
        }
        self.clone().spawn_dispose_with_task()
    }

    /// Awaits the background task without disposing the effect (mirrors the
    /// thenable form `await effect` in the TS reference).
    ///
    /// On task failure the already-collected disposables are cleaned up (the
    /// TS `task?.catch(dispose)` path) and the error is propagated.
    pub fn wait_task(self: &Rc<Self>) -> BoxFuture<'static, Result<(), Box<dyn Error>>> {
        if !self.has_task.get() {
            return Box::pin(async { Ok(()) });
        }
        let done = self.task_done.clone();
        let this = self.clone();
        Box::pin(async move {
            while !done.get() {
                tokio::task::yield_now().await;
            }
            let mut result = match &*this.task_result.borrow() {
                Some(Err(message)) => {
                    Err(Box::new(std::io::Error::other(message.clone())) as Box<dyn Error>)
                }
                _ => Ok(()),
            };
            if result.is_err() {
                let mut disposables = this.disposables.take();
                for item in disposables.drain(..).rev() {
                    let outcome = match item {
                        Disposable::Direct(disposer) => disposer().await,
                        Disposable::Effect(handle) => handle.dispose().await,
                    };
                    if result.is_ok() {
                        result = outcome;
                    }
                }
            }
            result
        })
    }

    /// Waits for the background task, then runs the disposer chain in the
    /// background; the returned future resolves when cleanup completes.
    fn spawn_dispose_with_task(self: Rc<Self>) -> BoxFuture<'static, Result<(), Box<dyn Error>>> {
        let done = self.task_done.clone();
        let this = self.clone();
        let cleanup_done = Rc::new(Cell::new(false));
        let cleanup_done_waiter = cleanup_done.clone();
        let join = tokio::task::spawn_local(async move {
            while !done.get() {
                tokio::task::yield_now().await;
            }
            let result = this.run_dispose_chain().await;
            cleanup_done_waiter.set(true);
            result
        });
        Box::pin(async move {
            while !cleanup_done.get() {
                tokio::task::yield_now().await;
            }
            join.await
                .unwrap_or_else(|error| Err(Box::new(error) as Box<dyn Error>))
        })
    }

    async fn run_dispose_chain(self: Rc<Self>) -> Result<(), Box<dyn Error>> {
        let mut result = match &*self.task_result.borrow() {
            Some(Err(message)) => {
                Err(Box::new(std::io::Error::other(message.clone())) as Box<dyn Error>)
            }
            _ => Ok(()),
        };
        let mut disposables = self.disposables.take();
        for item in disposables.drain(..).rev() {
            let outcome = match item {
                Disposable::Direct(disposer) => disposer().await,
                Disposable::Effect(handle) => handle.dispose().await,
            };
            if result.is_ok() {
                result = outcome;
            }
        }
        result
    }
}

impl fmt::Debug for EffectHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EffectHandle")
            .field("label", &self.label)
            .field("disposed", &self.is_disposed())
            .finish()
    }
}

#[derive(Default)]
pub(crate) struct Inertia {
    active: bool,
}

/// A plugin lifecycle unit.
///
/// Implements the TS fiber state machine: states (`PENDING` → `LOADING` →
/// `ACTIVE`/`FAILED` → `UNLOADING` → `DISPOSED`), inertia locking while
/// reload/unload tasks are in flight, inject-resolution epochs and the
/// effect executor.
pub struct Fiber {
    /// Fiber id; `None` once disposed (mirrors `uid: null`).
    pub uid: Cell<Option<u64>>,
    /// The context owned by this fiber.
    pub(crate) ctx: Rc<ContextInner>,
    /// Parent context (the root fiber has no parent).
    pub parent: Option<Context>,
    /// Current plugin config.
    pub config: RefCell<Option<Rc<dyn Any>>>,
    /// Current lifecycle state.
    pub state: Cell<FiberState>,
    /// Declared inject dependencies: name → config.
    pub inject: RefCell<HashMap<String, Option<Rc<dyn Any>>>>,
    /// The plugin runtime this fiber belongs to.
    pub(crate) runtime: RefCell<Option<Rc<Runtime>>>,
    /// Error captured from the last apply attempt.
    pub error: RefCell<Option<Box<dyn Error>>>,
    /// Current inject-resolution epoch.
    pub(crate) epoch: RefCell<Epoch>,
    /// Resolved service entries for the inject map.
    pub(crate) resolved: RefCell<HashMap<String, Rc<StoreEntry>>>,
    /// Ordered effect disposables (disposed in reverse order).
    pub(crate) disposables: RefCell<Vec<Disposable>>,
    /// Inertia lock state.
    pub(crate) inertia: RefCell<Inertia>,
    /// The dispose handle registered on the parent fiber.
    pub(crate) dispose: RefCell<Option<Rc<EffectHandle>>>,
    /// Fiber-level internal hooks.
    pub(crate) _hooks: RefCell<HashMap<String, Vec<InternalHook>>>,
    /// Config validator applied on updates.
    pub(crate) validator: RefCell<Option<ConfigValidator>>,
}

impl Fiber {
    /// Creates the root fiber of a context tree.
    pub(crate) fn root(ctx: Rc<ContextInner>) -> Rc<Self> {
        Rc::new(Fiber {
            uid: Cell::new(Some(0)),
            ctx,
            parent: None,
            config: RefCell::new(None),
            state: Cell::new(FiberState::Active),
            inject: RefCell::new(HashMap::new()),
            runtime: RefCell::new(None),
            error: RefCell::new(None),
            epoch: RefCell::new(Epoch::Active(String::new())),
            resolved: RefCell::new(HashMap::new()),
            disposables: RefCell::new(Vec::new()),
            inertia: RefCell::new(Inertia::default()),
            dispose: RefCell::new(None),
            _hooks: RefCell::new(HashMap::new()),
            validator: RefCell::new(None),
        })
    }

    /// Resolves the fiber name: runtime name → nearest parent fiber name →
    /// `root`.
    pub fn name(&self) -> String {
        let mut fiber: Option<&Fiber> = Some(self);
        while let Some(current) = fiber {
            if let Some(name) = current
                .runtime
                .borrow()
                .as_ref()
                .and_then(|r| r.name.clone())
            {
                return name;
            }
            fiber = current.parent.as_ref().map(|parent| &**parent.fiber());
        }
        "root".to_string()
    }

    /// Returns the context owned by this fiber.
    pub fn context(self: &Rc<Self>) -> Context {
        Context {
            inner: self.ctx.clone(),
            fiber: self.clone(),
        }
    }

    /// Asserts the fiber is not disposed.
    pub fn assert_active(&self) -> Result<(), CordisError> {
        if self.uid.get().is_none() {
            Err(CordisError::inactive_effect())
        } else {
            Ok(())
        }
    }

    /// Registers an effect on this fiber.
    ///
    /// The callback runs synchronously; async effects are awaited before the
    /// disposer chain runs. Disposal is idempotent.
    pub fn effect<F>(
        self: &Rc<Self>,
        execute: F,
        label: &str,
    ) -> Result<Rc<EffectHandle>, CordisError>
    where
        F: FnOnce() -> Effect,
    {
        self.assert_active()?;
        let handle = EffectHandle::new(label);
        let task = match self.run_effect(execute, &handle) {
            Ok(task) => task,
            Err(reason) => {
                // TS: `catch (reason) { dispose(); throw reason }` — already
                // collected disposables are cleaned up in the background
                // (requires a LocalSet; the error path only runs inside the
                // runtime).
                tokio::task::spawn_local(handle.dispose());
                return Err(CordisError {
                    code: "INVALID_EFFECT",
                    message: reason.to_string(),
                });
            }
        };
        if let Some(task) = task {
            // Async effects run in the background (requires a LocalSet) and
            // signal completion through `task_done`. The task runs under a
            // `JoinHandle` so a panic surfaces as an error instead of
            // silently aborting the completion handshake (which would hang
            // every waiter on `task_done`).
            let done = handle.task_done.clone();
            let task_result = handle.task_result.clone();
            let wrapped = Box::pin(async move {
                let task = tokio::task::spawn_local(task);
                let result = match task.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(format!("async effect task failed: {error}")),
                };
                *task_result.borrow_mut() = Some(result);
                done.set(true);
                Ok::<(), Box<dyn Error>>(())
            });
            handle.has_task.set(true);
            handle.task_done.set(false);
            tokio::task::spawn_local(wrapped);
        }
        self.disposables
            .borrow_mut()
            .push(Disposable::Effect(handle.clone()));
        Ok(handle)
    }

    /// Returns the ordered effect metadata tree (mirrors `fiber.getEffects()`).
    pub fn get_effects(&self) -> Vec<EffectMeta> {
        self.disposables
            .borrow()
            .iter()
            .filter_map(|item| match item {
                Disposable::Effect(handle) => Some(handle.meta()),
                Disposable::Direct(_) => None,
            })
            .collect()
    }

    /// Number of registered effect disposables (mirrors
    /// `fiber._disposables.length`).
    pub fn effect_count(&self) -> usize {
        self.disposables.borrow().len()
    }

    /// Awaits inertia completion and propagates apply errors.
    pub async fn wait(self: &Rc<Self>) -> Result<(), FiberError> {
        while self.inertia.borrow().active {
            tokio::task::yield_now().await;
        }
        if let Some(error) = &*self.error.borrow() {
            return Err(FiberError::new(error.to_string()));
        }
        Ok(())
    }

    /// Whether an inertia task is currently in flight.
    pub fn inertia_active(&self) -> bool {
        self.inertia.borrow().active
    }

    /// Restarts the fiber: unloads current effects, re-resolves injects and
    /// applies the plugin again.
    pub fn restart(self: &Rc<Self>) -> BoxFuture<'static, Result<(), FiberError>> {
        let this = self.clone();
        Box::pin(async move {
            this.assert_active()?;
            this.set_epoch(Epoch::Inactive);
            this.refresh();
            this.wait().await
        })
    }

    /// Updates the plugin config and restarts.
    pub fn update(
        self: &Rc<Self>,
        config: Option<Rc<dyn Any>>,
    ) -> BoxFuture<'static, Result<(), FiberError>> {
        self.update_with(config, false)
    }

    /// Updates the plugin config with the `noSave` flag and dispatches the
    /// `internal/update` waterfall (service hooks → fiber hooks → default).
    pub fn update_with(
        self: &Rc<Self>,
        config: Option<Rc<dyn Any>>,
        no_save: bool,
    ) -> BoxFuture<'static, Result<(), FiberError>> {
        let this = self.clone();
        Box::pin(async move {
            this.assert_active()?;
            if let Some(validator) = &*this.validator.borrow()
                && let Some(config) = &config
                && let Err(error) = validator(config)
            {
                this.log_error(&error);
                *this.error.borrow_mut() = Some(Box::new(FiberError::new(error.to_string())));
                return Err(FiberError::new(error.to_string()));
            }
            // Service-level global hooks first (e.g. the loader's write-back
            // hooks), then fiber-level hooks.
            let mut callbacks: Vec<EventCallback> = {
                let events = this.ctx.get_service::<crate::EventsService>("events");
                match events {
                    Some(events) => events.global_internal_update_hooks(),
                    None => Vec::new(),
                }
            };
            let fiber_hooks = this
                ._hooks
                .borrow()
                .get("internal/update")
                .cloned()
                .unwrap_or_default();
            callbacks.extend(fiber_hooks.into_iter().map(|hook| hook.callback));
            let applied = Rc::new(Cell::new(false));
            let this_for_tail = this.clone();
            let config_for_tail = config.clone();
            let applied_for_tail = applied.clone();
            let tail: WaterfallNext = Rc::new(move || {
                *this_for_tail.config.borrow_mut() = config_for_tail.clone();
                *this_for_tail.error.borrow_mut() = None;
                applied_for_tail.set(true);
                None
            });
            let args: Vec<Rc<dyn Any>> = vec![
                config.unwrap_or_else(|| Rc::new(()) as Rc<dyn Any>),
                Rc::new(no_save),
                {
                    let fiber_any: Rc<dyn Any> = this.clone();
                    fiber_any
                },
            ];
            let callbacks = Rc::new(RefCell::new(callbacks));
            let _ = run_waterfall_step(callbacks, args, tail);
            if applied.get() {
                this.restart().await
            } else {
                Ok(())
            }
        })
    }

    /// Registers a fiber-level internal hook (mirrors the `EventsService`
    /// constructor path for `internal/update`).
    pub fn register_internal_hook(
        self: &Rc<Self>,
        event: &str,
        callback: EventCallback,
        prepend: bool,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        let event = event.to_string();
        let this = self.clone();
        let effect_label = format!("ctx.on({event:?})");
        self.effect(
            move || {
                let hook = InternalHook {
                    callback: callback.clone(),
                };
                let mut hooks_borrow = this._hooks.borrow_mut();
                let list = hooks_borrow.entry(event.clone()).or_default();
                if prepend {
                    list.insert(0, hook);
                } else {
                    list.push(hook);
                }
                drop(hooks_borrow);
                let this = this.clone();
                let event = event.clone();
                Effect::Disposer(sync_disposer(move || {
                    if let Some(list) = this._hooks.borrow_mut().get_mut(&event) {
                        list.retain(|hook| !Rc::ptr_eq(&hook.callback, &callback));
                    }
                }))
            },
            &effect_label,
        )
    }

    /// Disposes the fiber (unregisters from its runtime, unloads effects).
    pub fn dispose(self: &Rc<Self>) -> BoxFuture<'static, ()> {
        if let Some(handle) = self.dispose.borrow().clone() {
            return Box::pin(async move {
                let _ = handle.dispose().await;
            });
        }
        // Root fibers dispose by restarting (mirrors `dispose = restart`).
        let this = self.clone();
        Box::pin(async move {
            let _ = this.restart().await;
        })
    }

    /// Re-checks one injected service and updates the resolved map.
    pub(crate) fn check_impl(self: &Rc<Self>, name: &str) {
        match self.ctx.lookup_strict(name) {
            Some(entry) => {
                let usable = match &entry.check {
                    Some(check) => {
                        let context = Context {
                            inner: self.ctx.clone(),
                            fiber: self.clone(),
                        };
                        check(&context)
                    }
                    None => true,
                };
                if usable {
                    self.resolved.borrow_mut().insert(name.to_string(), entry);
                } else {
                    self.resolved.borrow_mut().remove(name);
                }
            }
            None => {
                self.resolved.borrow_mut().remove(name);
            }
        }
    }

    /// Rebuilds the inject epoch from the resolved map and starts any needed
    /// state transition.
    pub(crate) fn refresh(self: &Rc<Self>) {
        let mut epoch_str = String::new();
        for name in self.inject.borrow().keys() {
            match self.resolved.borrow().get(name) {
                Some(entry) => match entry.fiber.upgrade().and_then(|fiber| fiber.uid.get()) {
                    Some(uid) => {
                        // Include the resolved realm in the epoch: a provider
                        // moving between isolate realms must restart its
                        // dependents (mirrors the TS "change provider" case).
                        let label = self
                            .ctx
                            .isolate_label(name)
                            .map(|label| label.to_string())
                            .unwrap_or_default();
                        epoch_str.push_str(&format!(":{uid}@{label}"));
                    }
                    None => {
                        self.set_epoch(Epoch::Inactive);
                        return;
                    }
                },
                None => {
                    self.set_epoch(Epoch::Inactive);
                    return;
                }
            }
        }
        self.set_epoch(Epoch::Active(epoch_str));
    }

    /// Applies a new epoch; when an inertia task is running the change is
    /// queued and picked up by the running task (inertia lock semantics).
    pub(crate) fn set_epoch(self: &Rc<Self>, epoch: Epoch) {
        if *self.epoch.borrow() == epoch {
            return;
        }
        let start_reload = epoch != Epoch::Inactive && *self.epoch.borrow() == Epoch::Inactive;
        let _ = std::mem::replace(&mut *self.epoch.borrow_mut(), epoch);
        let mut inertia = self.inertia.borrow_mut();
        if inertia.active {
            return;
        }
        inertia.active = true;
        drop(inertia);
        if start_reload {
            self.update_state(Some(FiberState::Loading));
            tokio::task::spawn_local(self.clone().reload());
        } else {
            self.update_state(Some(FiberState::Unloading));
            tokio::task::spawn_local(self.clone().unload());
        }
    }

    /// Runs the plugin apply callback and collects its effect disposers.
    async fn reload(self: Rc<Self>) {
        let target = self.epoch.borrow().clone();
        let task = {
            let runtime = self.runtime.borrow().clone();
            match runtime {
                Some(runtime) => {
                    let ctx = Context {
                        inner: self.ctx.clone(),
                        fiber: self.clone(),
                    };
                    let config = self.config.borrow().clone();
                    match self.run_apply(&ctx, &runtime.callback, &config) {
                        Ok(task) => task,
                        Err(reason) => {
                            self.log_error(&format!("{reason} at <{}>", self.name()));
                            *self.error.borrow_mut() =
                                Some(Box::new(FiberError::new(reason.to_string())));
                            *self.epoch.borrow_mut() = Epoch::Inactive;
                            None
                        }
                    }
                }
                None => None,
            }
        };
        if let Some(task) = task {
            // Run the apply task under a `JoinHandle` so a panic surfaces as
            // an error instead of unwinding `reload` and leaving the inertia
            // lock held forever (which would hang every `Fiber::wait`).
            let result = tokio::task::spawn_local(task).await;
            let reason = match result {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason.to_string()),
                Err(error) => Some(format!("plugin apply panicked: {error}")),
            };
            if let Some(reason) = reason {
                self.log_error(&format!("{reason} at <{}>", self.name()));
                *self.error.borrow_mut() = Some(Box::new(FiberError::new(reason)));
                *self.epoch.borrow_mut() = Epoch::Inactive;
            }
        }
        if *self.epoch.borrow() == target {
            self.inertia.borrow_mut().active = false;
            self.update_state(None);
        } else {
            self.update_state(Some(FiberState::Unloading));
            self.start_unload();
        }
    }

    async fn unload(self: Rc<Self>) {
        let disposables = self.disposables.take();
        for item in disposables.into_iter().rev() {
            let outcome = match item {
                // Run each disposer under a `JoinHandle` so a panicking
                // disposer cannot unwind `unload` and leave the inertia lock
                // held forever.
                Disposable::Direct(disposer) => tokio::task::spawn_local(disposer()).await,
                Disposable::Effect(handle) => tokio::task::spawn_local(handle.dispose()).await,
            };
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(reason)) => self.log_error(&reason),
                Err(error) => {
                    self.log_error(&format!("disposer panicked: {error}"));
                }
            }
        }
        if *self.epoch.borrow() == Epoch::Inactive {
            self.inertia.borrow_mut().active = false;
            self.update_state(None);
        } else {
            self.update_state(Some(FiberState::Loading));
            self.start_reload();
        }
    }

    fn start_reload(self: &Rc<Self>) {
        tokio::task::spawn_local(self.clone().reload());
    }

    fn start_unload(self: &Rc<Self>) {
        tokio::task::spawn_local(self.clone().unload());
    }

    fn get_state(&self) -> FiberState {
        if self.uid.get().is_none() {
            FiberState::Disposed
        } else if self.error.borrow().is_some() {
            FiberState::Failed
        } else if *self.epoch.borrow() != Epoch::Inactive {
            FiberState::Active
        } else {
            FiberState::Pending
        }
    }

    pub(crate) fn update_state(self: &Rc<Self>, explicit: Option<FiberState>) {
        let old = self.state.get();
        let new = explicit.unwrap_or_else(|| self.get_state());
        self.state.set(new);
        if old == new {
            return;
        }
        // `internal/status`: broadcast fiber state transitions, mirroring
        // fiber.ts `_updateState`. The payload carries the fiber and the
        // previous state.
        if let Some(events) = self
            .ctx
            .get_service_non_strict::<crate::EventsService>("events")
        {
            let ctx = Context {
                inner: self.ctx.clone(),
                fiber: self.clone(),
            };
            let fiber_any: Rc<dyn Any> = self.clone();
            let old_any: Rc<dyn Any> = Rc::new(old);
            events.emit(&ctx, "internal/status", &[fiber_any, old_any]);
        }
        // Notify consumers when crossing the ACTIVE boundary.
        let toggled_active = (old == FiberState::Active) != (new == FiberState::Active);
        if toggled_active {
            self.notify_provided();
        }
    }

    fn notify_provided(self: &Rc<Self>) {
        let names: Vec<String> = self
            .ctx
            .store
            .borrow()
            .by_label
            .values()
            .filter(|entry| {
                entry
                    .fiber
                    .upgrade()
                    .map(|fiber| Rc::ptr_eq(&fiber, self))
                    .unwrap_or(false)
            })
            .map(|entry| entry.name.clone())
            .collect();
        if names.is_empty() {
            return;
        }
        if let Some(registry) = self.ctx.get_service::<crate::RegistryService>("registry") {
            let provider = Context {
                inner: self.ctx.clone(),
                fiber: self.clone(),
            };
            for name in names {
                let _ = registry.notify(&name, &provider);
            }
        }
    }

    pub(crate) fn log_error(&self, reason: &dyn fmt::Display) {
        if let Some(logger) = self.ctx.get_service::<crate::LoggerService>("logger") {
            logger.error(reason);
        }
    }

    /// Executes an effect callback and collects disposers into the handle.
    fn run_effect<F>(
        self: &Rc<Self>,
        execute: F,
        handle: &Rc<EffectHandle>,
    ) -> Result<EffectTask, Box<dyn Error>>
    where
        F: FnOnce() -> Effect,
    {
        let fiber = self.clone();
        let collect = |fiber: &Rc<Self>, handle: &Rc<EffectHandle>, item: Disposable| {
            if let Disposable::Effect(nested) = &item {
                handle.meta.borrow_mut().children.push(nested.meta());
                let mut list = fiber.disposables.borrow_mut();
                if let Some(position) = list.iter().position(
                    |entry| matches!(entry, Disposable::Effect(h) if Rc::ptr_eq(h, nested)),
                ) {
                    list.remove(position);
                }
            }
            handle.collect(item);
        };
        match execute() {
            Effect::None => Ok(None),
            Effect::Disposer(disposer) => {
                collect(&fiber, handle, Disposable::Direct(disposer));
                Ok(None)
            }
            Effect::Nested(nested) => {
                collect(&fiber, handle, Disposable::Effect(nested));
                Ok(None)
            }
            Effect::Async(future) => {
                let handle = handle.clone();
                let fiber = fiber.clone();
                Ok(Some(Box::pin(async move {
                    match future.await {
                        Ok(disposer) => {
                            collect(&fiber, &handle, Disposable::Direct(disposer));
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                })))
            }
            Effect::Iterable(items) => {
                for item in items {
                    match item {
                        Ok(EffectItem::Disposer(disposer)) => {
                            collect(&fiber, handle, Disposable::Direct(disposer));
                        }
                        Ok(EffectItem::Nested(nested)) => {
                            collect(&fiber, handle, Disposable::Effect(nested));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(None)
            }
            Effect::AsyncIterable(stream) => {
                let handle = handle.clone();
                let fiber = fiber.clone();
                let mut stream = stream;
                let mut stream_pending = false;
                Ok(Some(Box::pin(async move {
                    poll_fn(|cx| {
                        loop {
                            if !stream_pending && handle.is_disposed() {
                                return Poll::Ready(Ok(()));
                            }
                            match stream.as_mut().poll_next(cx) {
                                Poll::Ready(Some(Ok(disposer))) => {
                                    collect(&fiber, &handle, Disposable::Direct(disposer));
                                    stream_pending = false;
                                }
                                Poll::Ready(Some(Err(error))) => {
                                    return Poll::Ready(Err(error));
                                }
                                Poll::Ready(None) => return Poll::Ready(Ok(())),
                                Poll::Pending => {
                                    stream_pending = true;
                                    return Poll::Pending;
                                }
                            }
                        }
                    })
                    .await
                })))
            }
            Effect::Error(reason) => Err(reason),
        }
    }

    /// Executes the plugin apply callback, collecting disposers into the
    /// fiber's own disposables list.
    fn run_apply(
        self: &Rc<Self>,
        ctx: &Context,
        callback: &ApplyFn,
        config: &Option<Rc<dyn Any>>,
    ) -> Result<EffectTask, Box<dyn Error>> {
        let empty: Rc<dyn Any> = Rc::new(());
        let config = config.as_ref().unwrap_or(&empty);
        let target = self.epoch.borrow().clone();
        let collect = |this: &Rc<Self>, item: Disposable| {
            this.disposables.borrow_mut().push(item);
        };
        let effect =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(ctx, config)))
                .map_err(Self::apply_panic_error)?;
        match effect {
            Effect::None => Ok(None),
            Effect::Disposer(disposer) => {
                collect(self, Disposable::Direct(disposer));
                Ok(None)
            }
            Effect::Nested(nested) => {
                collect(self, Disposable::Effect(nested));
                Ok(None)
            }
            Effect::Async(future) => {
                let this = self.clone();
                Ok(Some(Box::pin(async move {
                    match future.await {
                        Ok(disposer) => {
                            this.disposables
                                .borrow_mut()
                                .push(Disposable::Direct(disposer));
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                })))
            }
            Effect::Iterable(items) => {
                for item in items {
                    match item {
                        Ok(EffectItem::Disposer(disposer)) => {
                            self.disposables
                                .borrow_mut()
                                .push(Disposable::Direct(disposer));
                        }
                        Ok(EffectItem::Nested(nested)) => {
                            self.disposables
                                .borrow_mut()
                                .push(Disposable::Effect(nested));
                        }
                        Err(error) => return Err(error),
                    }
                }
                Ok(None)
            }
            Effect::AsyncIterable(stream) => {
                let this = self.clone();
                let mut stream = stream;
                let mut stream_pending = false;
                Ok(Some(Box::pin(async move {
                    poll_fn(|cx| {
                        loop {
                            if !stream_pending && *this.epoch.borrow() != target {
                                return Poll::Ready(Ok(()));
                            }
                            match stream.as_mut().poll_next(cx) {
                                Poll::Ready(Some(Ok(disposer))) => {
                                    this.disposables
                                        .borrow_mut()
                                        .push(Disposable::Direct(disposer));
                                    stream_pending = false;
                                }
                                Poll::Ready(Some(Err(error))) => {
                                    return Poll::Ready(Err(error));
                                }
                                Poll::Ready(None) => return Poll::Ready(Ok(())),
                                Poll::Pending => {
                                    stream_pending = true;
                                    return Poll::Pending;
                                }
                            }
                        }
                    })
                    .await
                })))
            }
            Effect::Error(reason) => Err(reason),
        }
    }

    /// Converts a panicked apply callback into an error (mirrors the TS
    /// try/catch around the plugin entry).
    fn apply_panic_error(payload: Box<dyn Any + Send>) -> Box<dyn Error> {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "plugin apply panicked".to_string());
        Box::new(FiberError::new(message))
    }
}

impl fmt::Debug for Fiber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fiber")
            .field("uid", &self.uid.get())
            .field("name", &self.name())
            .field("state", &self.state.get())
            .finish()
    }
}

/// Creates a disposer from a sync closure (public convenience helper).
pub fn disposer<F>(f: F) -> Disposer
where
    F: FnOnce() + 'static,
{
    sync_disposer(f)
}
