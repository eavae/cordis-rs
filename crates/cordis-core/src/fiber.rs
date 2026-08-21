//! Fiber lifecycle state machine and effect executor.

use std::any::Any;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use crate::context::{Context, ContextInner, StoreEntry};
use crate::error::ConfigValidator;
use crate::events::{EventCallback, WaterfallNext, run_waterfall_step};
use crate::registry::Runtime;
use crate::service::{ApplyFn, BoxError, BoxFuture, Disposer, Effect, EffectItem, sync_disposer};
use tokio::sync::Notify;

/// Lifecycle state of a [`Fiber`] (mirrors `FiberState` in the TS reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FiberState {
    /// The fiber is scheduled but not yet loading.
    Pending = 0,
    /// The plugin entry is being applied.
    Loading = 1,
    /// The plugin entry is active.
    Active = 2,
    /// The plugin entry failed to apply.
    Failed = 3,
    /// The fiber has been disposed.
    Disposed = 4,
    /// The fiber is being unloaded.
    Unloading = 5,
}

impl FiberState {
    /// Decodes a state stored in the fiber's `AtomicU8`.
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Pending,
            1 => Self::Loading,
            2 => Self::Active,
            3 => Self::Failed,
            4 => Self::Disposed,
            _ => Self::Unloading,
        }
    }
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
        Self {
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
        Self {
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
        Self::new(error.message)
    }
}

/// Effect metadata tree entry (mirrors `EffectMeta` in fiber.ts).
#[derive(Clone, Debug, PartialEq)]
pub struct EffectMeta {
    /// The label passed to [`Fiber::effect`].
    pub label: String,
    /// Nested effects collected during this effect's execution.
    pub children: Vec<Self>,
}

/// An entry of a fiber's (or an effect's) disposables list.
pub(crate) enum Disposable {
    /// A plain disposer (from a plugin apply or a yielded disposer).
    Direct(Disposer),
    /// A nested effect handle.
    Effect(Arc<EffectHandle>),
}

/// A fiber-level internal hook (e.g. `internal/update`).
#[derive(Clone)]
pub(crate) struct InternalHook {
    pub callback: EventCallback,
}

/// The optional background task produced by an async effect or apply.
pub(crate) type EffectTask = Option<BoxFuture<'static, Result<(), BoxError>>>;

/// Idempotent effect handle returned by [`Fiber::effect`].
pub struct EffectHandle {
    /// Label used for diagnostics and effect metadata.
    pub label: String,
    epoch: AtomicBool,
    disposables: Mutex<Vec<Disposable>>,
    has_task: AtomicBool,
    task_done: Arc<AtomicBool>,
    task_notify: Arc<Notify>,
    task_result: Arc<Mutex<Option<Result<(), String>>>>,
    meta: Mutex<EffectMeta>,
}

impl EffectHandle {
    fn new(label: &str) -> Arc<Self> {
        Arc::new(Self {
            label: label.to_string(),
            epoch: AtomicBool::new(true),
            disposables: Mutex::new(Vec::new()),
            has_task: AtomicBool::new(false),
            task_done: Arc::new(AtomicBool::new(true)),
            task_notify: Arc::new(Notify::new()),
            task_result: Arc::new(Mutex::new(None)),
            meta: Mutex::new(EffectMeta {
                label: label.to_string(),
                children: Vec::new(),
            }),
        })
    }

    fn collect(&self, item: Disposable) {
        self.disposables.lock().unwrap().push(item);
    }

    /// Whether the effect has already been disposed.
    pub fn is_disposed(&self) -> bool {
        !self.epoch.load(Ordering::Acquire)
    }

    /// The metadata tree of this effect.
    pub fn meta(&self) -> EffectMeta {
        self.meta.lock().unwrap().clone()
    }

    /// Runs the disposer chain (idempotent) and resolves with the first
    /// error, if any. Async effects are awaited before the chain runs.
    pub fn dispose(self: &Arc<Self>) -> BoxFuture<'static, Result<(), BoxError>> {
        if !self.epoch.swap(false, Ordering::AcqRel) {
            return Box::pin(async { Ok(()) });
        }
        if !self.has_task.load(Ordering::Acquire) {
            return Box::pin(self.clone().run_dispose_chain());
        }
        self.clone().spawn_dispose_with_task()
    }

    /// Awaits the background task without disposing the effect (mirrors the
    /// thenable form `await effect` in the TS reference).
    ///
    /// On task failure the already-collected disposables are cleaned up (the
    /// TS `task?.catch(dispose)` path) and the error is propagated.
    pub fn wait_task(self: &Arc<Self>) -> BoxFuture<'static, Result<(), BoxError>> {
        if !self.has_task.load(Ordering::Acquire) {
            return Box::pin(async { Ok(()) });
        }
        let done = self.task_done.clone();
        let task_notify = self.task_notify.clone();
        let this = self.clone();
        Box::pin(async move {
            while !done.load(Ordering::Acquire) {
                let notified = task_notify.notified();
                if done.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
            let mut result = match &*this.task_result.lock().unwrap() {
                Some(Err(message)) => {
                    Err(Box::new(std::io::Error::other(message.clone())) as BoxError)
                }
                _ => Ok(()),
            };
            if result.is_err() {
                let mut disposables = std::mem::take(&mut *this.disposables.lock().unwrap());
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
    fn spawn_dispose_with_task(self: Arc<Self>) -> BoxFuture<'static, Result<(), BoxError>> {
        let done = self.task_done.clone();
        let task_notify = self.task_notify.clone();
        let this = self;
        let cleanup_done = Arc::new(AtomicBool::new(false));
        let cleanup_done_waiter = cleanup_done.clone();
        let cleanup_notify = Arc::new(Notify::new());
        let cleanup_notify_waiter = cleanup_notify.clone();
        let join = tokio::task::spawn_local(async move {
            while !done.load(Ordering::Acquire) {
                let notified = task_notify.notified();
                if done.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
            let result = this.run_dispose_chain().await;
            cleanup_done_waiter.store(true, Ordering::Release);
            cleanup_notify_waiter.notify_waiters();
            result
        });
        Box::pin(async move {
            while !cleanup_done.load(Ordering::Acquire) {
                let notified = cleanup_notify.notified();
                if cleanup_done.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
            join.await
                .unwrap_or_else(|error| Err(Box::new(error) as BoxError))
        })
    }

    async fn run_dispose_chain(self: Arc<Self>) -> Result<(), BoxError> {
        let mut result = match &*self.task_result.lock().unwrap() {
            Some(Err(message)) => Err(Box::new(std::io::Error::other(message.clone())) as BoxError),
            _ => Ok(()),
        };
        let mut disposables = std::mem::take(&mut *self.disposables.lock().unwrap());
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
    pub(crate) uid: AtomicU64,
    /// The context owned by this fiber.
    pub(crate) ctx: Arc<ContextInner>,
    /// Parent context (the root fiber has no parent).
    pub parent: Option<Context>,
    /// Current plugin config.
    pub config: Mutex<Option<Arc<dyn Any + Send + Sync>>>,
    /// Current lifecycle state.
    pub(crate) state: AtomicU8,
    /// Declared inject dependencies: name → config.
    pub inject: Mutex<HashMap<String, Option<Arc<dyn Any + Send + Sync>>>>,
    /// The plugin runtime this fiber belongs to.
    pub(crate) runtime: Option<Arc<Runtime>>,
    /// Error captured from the last apply attempt.
    pub error: Mutex<Option<BoxError>>,
    /// Current inject-resolution epoch.
    pub(crate) epoch: Mutex<Epoch>,
    /// Resolved service entries for the inject map.
    pub(crate) resolved: Mutex<HashMap<String, Arc<StoreEntry>>>,
    /// Ordered effect disposables (disposed in reverse order).
    pub(crate) disposables: Mutex<Vec<Disposable>>,
    /// Inertia lock state.
    pub(crate) inertia: Mutex<Inertia>,
    /// Notifies waiters when the inertia lock is released.
    pub(crate) inertia_notify: Arc<Notify>,
    /// The dispose handle registered on the parent fiber.
    pub(crate) dispose: Mutex<Option<Arc<EffectHandle>>>,
    /// Fiber-level internal hooks.
    pub(crate) _hooks: Mutex<HashMap<String, Vec<InternalHook>>>,
    /// Config validator applied on updates.
    pub(crate) validator: Mutex<Option<ConfigValidator>>,
}

impl Fiber {
    /// Creates the root fiber of a context tree.
    pub(crate) fn root(ctx: Arc<ContextInner>) -> Arc<Self> {
        Arc::new(Self {
            uid: AtomicU64::new(0),
            ctx,
            parent: None,
            config: Mutex::new(None),
            state: AtomicU8::new(FiberState::Active as u8),
            inject: Mutex::new(HashMap::new()),
            runtime: None,
            error: Mutex::new(None),
            epoch: Mutex::new(Epoch::Active(String::new())),
            resolved: Mutex::new(HashMap::new()),
            disposables: Mutex::new(Vec::new()),
            inertia: Mutex::new(Inertia::default()),
            inertia_notify: Arc::new(Notify::new()),
            dispose: Mutex::new(None),
            _hooks: Mutex::new(HashMap::new()),
            validator: Mutex::new(None),
        })
    }

    /// The fiber id; `None` once disposed (`u64::MAX` is the sentinel).
    pub fn uid(&self) -> Option<u64> {
        let uid = self.uid.load(Ordering::Acquire);
        if uid == u64::MAX { None } else { Some(uid) }
    }

    /// Sets the fiber id (`None` marks the fiber disposed).
    pub(crate) fn set_uid(&self, uid: Option<u64>) {
        self.uid.store(uid.unwrap_or(u64::MAX), Ordering::Release);
    }

    /// The current lifecycle state.
    pub fn state(&self) -> FiberState {
        FiberState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Sets the lifecycle state.
    pub(crate) fn set_state(&self, state: FiberState) {
        self.state.store(state as u8, Ordering::Release);
    }

    /// Resolves the fiber name: runtime name → nearest parent fiber name →
    /// `root`.
    pub fn name(&self) -> String {
        let mut fiber: Option<&Self> = Some(self);
        while let Some(current) = fiber {
            if let Some(name) = current.runtime.as_ref().and_then(|r| r.name.clone()) {
                return name;
            }
            fiber = current.parent.as_ref().map(|parent| &**parent.fiber());
        }
        "root".to_string()
    }

    /// Returns the context owned by this fiber.
    pub fn context(self: &Arc<Self>) -> Context {
        Context {
            inner: self.ctx.clone(),
            fiber: self.clone(),
        }
    }

    /// Asserts the fiber is not disposed.
    pub fn assert_active(&self) -> Result<(), CordisError> {
        if self.uid().is_none() {
            Err(CordisError::inactive_effect())
        } else {
            Ok(())
        }
    }

    /// Registers an effect on this fiber.
    ///
    /// The callback runs synchronously; async effects are awaited before the
    /// disposer chain runs. Disposal is idempotent. Panics in the callback
    /// are contained at this boundary: the effect is disposed and the error
    /// is returned to the caller instead of unwinding into framework state
    /// (mirrors the `try/catch` around the TS effect registration).
    pub fn effect<F>(
        self: &Arc<Self>,
        execute: F,
        label: &str,
    ) -> Result<Arc<EffectHandle>, CordisError>
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
            let task_notify = handle.task_notify.clone();
            let task_result = handle.task_result.clone();
            let wrapped = Box::pin(async move {
                let task = tokio::task::spawn_local(task);
                let result = match task.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(error) => Err(format!("async effect task failed: {error}")),
                };
                *task_result.lock().unwrap() = Some(result);
                done.store(true, Ordering::Release);
                task_notify.notify_waiters();
                Ok::<(), BoxError>(())
            });
            handle.has_task.store(true, Ordering::Release);
            handle.task_done.store(false, Ordering::Release);
            tokio::task::spawn_local(wrapped);
        }
        self.disposables
            .lock()
            .unwrap()
            .push(Disposable::Effect(handle.clone()));
        Ok(handle)
    }

    /// Returns the ordered effect metadata tree (mirrors `fiber.getEffects()`).
    pub fn get_effects(&self) -> Vec<EffectMeta> {
        self.disposables
            .lock()
            .unwrap()
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
        self.disposables.lock().unwrap().len()
    }

    /// Awaits inertia completion and propagates apply errors.
    pub async fn wait(self: &Arc<Self>) -> Result<(), FiberError> {
        while self.inertia.lock().unwrap().active {
            let notified = self.inertia_notify.notified();
            if !self.inertia.lock().unwrap().active {
                break;
            }
            notified.await;
        }
        if let Some(error) = &*self.error.lock().unwrap() {
            return Err(FiberError::new(error.to_string()));
        }
        Ok(())
    }

    /// Whether an inertia task is currently in flight.
    pub fn inertia_active(&self) -> bool {
        self.inertia.lock().unwrap().active
    }

    /// Restarts the fiber: unloads current effects, re-resolves injects and
    /// applies the plugin again.
    pub fn restart(self: &Arc<Self>) -> BoxFuture<'static, Result<(), FiberError>> {
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
        self: &Arc<Self>,
        config: Option<Arc<dyn Any + Send + Sync>>,
    ) -> BoxFuture<'static, Result<(), FiberError>> {
        self.update_with(config, false)
    }

    /// Updates the plugin config with the `noSave` flag and dispatches the
    /// `internal/update` waterfall (service hooks → fiber hooks → default).
    pub fn update_with(
        self: &Arc<Self>,
        config: Option<Arc<dyn Any + Send + Sync>>,
        no_save: bool,
    ) -> BoxFuture<'static, Result<(), FiberError>> {
        let this = self.clone();
        Box::pin(async move {
            this.assert_active()?;
            let validator = this.validator.lock().unwrap().clone();
            if let Some(validator) = validator.as_ref()
                && let Some(config) = &config
                && let Err(error) = validator(config)
            {
                this.log_error(&error);
                *this.error.lock().unwrap() = Some(Box::new(FiberError::new(error.to_string())));
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
                .lock()
                .unwrap()
                .get("internal/update")
                .cloned()
                .unwrap_or_default();
            callbacks.extend(fiber_hooks.into_iter().map(|hook| hook.callback));
            let applied = Arc::new(AtomicBool::new(false));
            let this_for_tail = this.clone();
            let config_for_tail = config.clone();
            let applied_for_tail = applied.clone();
            let tail: WaterfallNext = Arc::new(move || {
                let this = this_for_tail.clone();
                let config = config_for_tail.clone();
                let applied = applied_for_tail.clone();
                Box::pin(async move {
                    *this.config.lock().unwrap() = config;
                    *this.error.lock().unwrap() = None;
                    applied.store(true, Ordering::Release);
                    Ok(None)
                })
            });
            let args: Vec<Arc<dyn Any + Send + Sync>> = vec![
                config.unwrap_or_else(|| Arc::new(()) as Arc<dyn Any + Send + Sync>),
                Arc::new(no_save),
                {
                    let fiber_any: Arc<dyn Any + Send + Sync> = this.clone();
                    fiber_any
                },
            ];
            let callbacks = Arc::new(Mutex::new(callbacks));
            let _ = run_waterfall_step(callbacks, args, tail).await;
            if applied.load(Ordering::Acquire) {
                this.restart().await
            } else {
                Ok(())
            }
        })
    }

    /// Registers a fiber-level internal hook (mirrors the `EventsService`
    /// constructor path for `internal/update`).
    pub fn register_internal_hook(
        self: &Arc<Self>,
        event: &str,
        callback: EventCallback,
        prepend: bool,
    ) -> Result<Arc<EffectHandle>, CordisError> {
        let event = event.to_string();
        let this = self.clone();
        let effect_label = format!("ctx.on({event:?})");
        self.effect(
            move || {
                let hook = InternalHook {
                    callback: callback.clone(),
                };
                let mut hooks_borrow = this._hooks.lock().unwrap();
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
                    if let Some(list) = this._hooks.lock().unwrap().get_mut(&event) {
                        list.retain(|hook| !Arc::ptr_eq(&hook.callback, &callback));
                    }
                }))
            },
            &effect_label,
        )
    }

    /// Disposes the fiber (unregisters from its runtime, unloads effects).
    pub fn dispose(self: &Arc<Self>) -> BoxFuture<'static, ()> {
        if let Some(handle) = self.dispose.lock().unwrap().clone() {
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
    pub(crate) fn check_impl(self: &Arc<Self>, name: &str) {
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
                    self.resolved
                        .lock()
                        .unwrap()
                        .insert(name.to_string(), entry);
                } else {
                    self.resolved.lock().unwrap().remove(name);
                }
            }
            None => {
                self.resolved.lock().unwrap().remove(name);
            }
        }
    }

    /// Rebuilds the inject epoch from the resolved map and starts any needed
    /// state transition.
    pub(crate) fn refresh(self: &Arc<Self>) {
        let mut epoch_str = String::new();
        for name in self.inject.lock().unwrap().keys() {
            match self.resolved.lock().unwrap().get(name) {
                Some(entry) => match entry.fiber.upgrade().and_then(|fiber| fiber.uid()) {
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
    pub(crate) fn set_epoch(self: &Arc<Self>, epoch: Epoch) {
        if *self.epoch.lock().unwrap() == epoch {
            return;
        }
        let start_reload =
            epoch != Epoch::Inactive && *self.epoch.lock().unwrap() == Epoch::Inactive;
        let _ = std::mem::replace(&mut *self.epoch.lock().unwrap(), epoch);
        let mut inertia = self.inertia.lock().unwrap();
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
    async fn reload(self: Arc<Self>) {
        let target = self.epoch.lock().unwrap().clone();
        let task = {
            let runtime = self.runtime.clone();
            match runtime {
                Some(runtime) => {
                    let ctx = Context {
                        inner: self.ctx.clone(),
                        fiber: self.clone(),
                    };
                    let config = self.config.lock().unwrap().clone();
                    match self.run_apply(&ctx, &runtime.callback, &config) {
                        Ok(task) => task,
                        Err(reason) => {
                            self.log_error(&format!("{reason} at <{}>", self.name()));
                            *self.error.lock().unwrap() =
                                Some(Box::new(FiberError::new(reason.to_string())));
                            *self.epoch.lock().unwrap() = Epoch::Inactive;
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
                *self.error.lock().unwrap() = Some(Box::new(FiberError::new(reason)));
                *self.epoch.lock().unwrap() = Epoch::Inactive;
            }
        }
        if *self.epoch.lock().unwrap() == target {
            self.inertia.lock().unwrap().active = false;
            self.inertia_notify.notify_waiters();
            self.update_state(None);
        } else {
            self.update_state(Some(FiberState::Unloading));
            self.start_unload();
        }
    }

    async fn unload(self: Arc<Self>) {
        let disposables = std::mem::take(&mut *self.disposables.lock().unwrap());
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
        if *self.epoch.lock().unwrap() == Epoch::Inactive {
            self.inertia.lock().unwrap().active = false;
            self.inertia_notify.notify_waiters();
            self.update_state(None);
        } else {
            self.update_state(Some(FiberState::Loading));
            self.start_reload();
        }
    }

    fn start_reload(self: &Arc<Self>) {
        tokio::task::spawn_local(self.clone().reload());
    }

    fn start_unload(self: &Arc<Self>) {
        tokio::task::spawn_local(self.clone().unload());
    }

    fn get_state(&self) -> FiberState {
        if self.uid().is_none() {
            FiberState::Disposed
        } else if self.error.lock().unwrap().is_some() {
            FiberState::Failed
        } else if *self.epoch.lock().unwrap() != Epoch::Inactive {
            FiberState::Active
        } else {
            FiberState::Pending
        }
    }

    pub(crate) fn update_state(self: &Arc<Self>, explicit: Option<FiberState>) {
        let old = self.state();
        let new = explicit.unwrap_or_else(|| self.get_state());
        self.set_state(new);
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
            let fiber_any: Arc<dyn Any + Send + Sync> = self.clone();
            let old_any: Arc<dyn Any + Send + Sync> = Arc::new(old);
            events.emit(&ctx, "internal/status", &[fiber_any, old_any]);
        }
        // Notify consumers when crossing the ACTIVE boundary.
        let toggled_active = (old == FiberState::Active) != (new == FiberState::Active);
        if toggled_active {
            self.notify_provided();
        }
    }

    fn notify_provided(self: &Arc<Self>) {
        let names: Vec<String> = self
            .ctx
            .store
            .load_full()
            .by_label
            .values()
            .filter(|entry| {
                entry
                    .fiber
                    .upgrade()
                    .is_some_and(|fiber| Arc::ptr_eq(&fiber, self))
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
    ///
    /// A panicking callback is converted into an error (see [`Self::effect`])
    /// so the unwind never reaches framework state held by the caller.
    fn run_effect<F>(
        self: &Arc<Self>,
        execute: F,
        handle: &Arc<EffectHandle>,
    ) -> Result<EffectTask, BoxError>
    where
        F: FnOnce() -> Effect,
    {
        let fiber = self.clone();
        let collect = |fiber: &Arc<Self>, handle: &Arc<EffectHandle>, item: Disposable| {
            if let Disposable::Effect(nested) = &item {
                handle.meta.lock().unwrap().children.push(nested.meta());
                let mut list = fiber.disposables.lock().unwrap();
                if let Some(position) = list.iter().position(
                    |entry| matches!(entry, Disposable::Effect(h) if Arc::ptr_eq(h, nested)),
                ) {
                    list.remove(position);
                }
            }
            handle.collect(item);
        };
        let effect = std::panic::catch_unwind(std::panic::AssertUnwindSafe(execute))
            .map_err(Self::panic_error)?;
        match effect {
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
                let fiber = fiber;
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
                let fiber = fiber;
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
        self: &Arc<Self>,
        ctx: &Context,
        callback: &ApplyFn,
        config: &Option<Arc<dyn Any + Send + Sync>>,
    ) -> Result<EffectTask, BoxError> {
        let empty: Arc<dyn Any + Send + Sync> = Arc::new(());
        let config = config.as_ref().unwrap_or(&empty);
        let target = self.epoch.lock().unwrap().clone();
        let collect = |this: &Arc<Self>, item: Disposable| {
            this.disposables.lock().unwrap().push(item);
        };
        let effect =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(ctx, config)))
                .map_err(Self::panic_error)?;
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
                                .lock()
                                .unwrap()
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
                                .lock()
                                .unwrap()
                                .push(Disposable::Direct(disposer));
                        }
                        Ok(EffectItem::Nested(nested)) => {
                            self.disposables
                                .lock()
                                .unwrap()
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
                            if !stream_pending && *this.epoch.lock().unwrap() != target {
                                return Poll::Ready(Ok(()));
                            }
                            match stream.as_mut().poll_next(cx) {
                                Poll::Ready(Some(Ok(disposer))) => {
                                    this.disposables
                                        .lock()
                                        .unwrap()
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

    /// Converts a panicked user callback into an error (mirrors the TS
    /// try/catch around the plugin entry and the effect registration).
    fn panic_error(payload: Box<dyn Any + Send>) -> BoxError {
        let message = payload
            .downcast_ref::<&str>()
            .map(|message| message.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "plugin callback panicked".to_string());
        Box::new(FiberError::new(message))
    }
}

impl fmt::Debug for Fiber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fiber")
            .field("uid", &self.uid())
            .field("name", &self.name())
            .field("state", &self.state())
            .finish()
    }
}

/// Creates a disposer from a sync closure (public convenience helper).
pub fn disposer<F>(f: F) -> Disposer
where
    F: FnOnce() + Send + 'static,
{
    sync_disposer(f)
}
