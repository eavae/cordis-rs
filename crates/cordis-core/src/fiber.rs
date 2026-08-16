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
use crate::registry::Runtime;
use crate::service::{ApplyFn, BoxFuture, Disposer, Effect, sync_disposer};

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
///
/// Story card B12 replaces this with the full error-chain model.
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

/// Idempotent effect handle returned by [`Fiber::effect`].
pub struct EffectHandle {
    /// Label used for diagnostics and effect metadata.
    pub label: String,
    epoch: Cell<bool>,
    disposables: RefCell<Vec<Disposer>>,
    task: RefCell<Option<BoxFuture<'static, ()>>>,
}

impl EffectHandle {
    fn new(label: &str) -> Rc<Self> {
        Rc::new(EffectHandle {
            label: label.to_string(),
            epoch: Cell::new(true),
            disposables: RefCell::new(Vec::new()),
            task: RefCell::new(None),
        })
    }

    fn collect(&self, disposer: Disposer) {
        self.disposables.borrow_mut().push(disposer);
    }

    /// Whether the effect has already been disposed.
    pub fn is_disposed(&self) -> bool {
        !self.epoch.get()
    }

    /// Runs the disposer chain (idempotent) and resolves when it completes.
    pub fn dispose(self: &Rc<Self>) -> BoxFuture<'static, ()> {
        if !self.epoch.replace(false) {
            return Box::pin(async {});
        }
        let this = self.clone();
        Box::pin(async move {
            if let Some(task) = this.task.take() {
                task.await;
            }
            let mut disposables = this.disposables.take();
            for disposer in disposables.drain(..).rev() {
                let _ = disposer().await;
            }
        })
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
    /// Ordered effect disposers (disposed in reverse order).
    pub(crate) disposables: RefCell<Vec<Disposer>>,
    /// Inertia lock state.
    pub(crate) inertia: RefCell<Inertia>,
    /// The dispose handle registered on the parent fiber.
    pub(crate) dispose: RefCell<Option<Rc<EffectHandle>>>,
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
                // TS: `catch (reason) { dispose(); throw reason }`
                // The cleanup future is dropped: this error path runs in a
                // synchronous context (B3 refines async cleanup ordering).
                std::mem::drop(handle.dispose());
                return Err(CordisError {
                    code: "INVALID_EFFECT",
                    message: reason.to_string(),
                });
            }
        };
        handle.task.replace(task);
        let handle_for_fiber = handle.clone();
        self.disposables.borrow_mut().push(Box::new(move || {
            let handle = handle_for_fiber.clone();
            Box::pin(async move {
                handle.dispose().await;
                Ok(())
            })
        }));
        Ok(handle)
    }

    /// Returns the ordered effect metadata tree (story card B3).
    pub fn get_effects(&self) -> Vec<Rc<EffectHandle>> {
        // B3 replaces this with a metadata tree; B2 exposes the raw handles.
        Vec::new()
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

    /// Updates the plugin config and restarts (schema validation in B6/B12).
    pub fn update(
        self: &Rc<Self>,
        config: Option<Rc<dyn Any>>,
    ) -> BoxFuture<'static, Result<(), FiberError>> {
        let this = self.clone();
        Box::pin(async move {
            this.assert_active()?;
            *this.config.borrow_mut() = config;
            *this.error.borrow_mut() = None;
            this.restart().await
        })
    }

    /// Disposes the fiber (unregisters from its runtime, unloads effects).
    pub fn dispose(self: &Rc<Self>) -> BoxFuture<'static, ()> {
        if let Some(handle) = &*self.dispose.borrow() {
            return handle.dispose();
        }
        // Root fibers dispose by restarting (mirrors `dispose = restart`).
        let this = self.clone();
        Box::pin(async move {
            let _ = this.restart().await;
        })
    }

    /// Re-checks one injected service and updates the resolved map.
    pub(crate) fn check_impl(&self, name: &str) {
        match self.ctx.lookup_strict(name) {
            Some(entry) => {
                self.resolved.borrow_mut().insert(name.to_string(), entry);
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
                    Some(uid) => epoch_str.push_str(&format!(":{uid}")),
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
            self.state.set(FiberState::Loading);
            self.update_state(Some(FiberState::Loading));
            tokio::task::spawn_local(self.clone().reload());
        } else {
            self.state.set(FiberState::Unloading);
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
                            self.log_error(&reason);
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
            task.await;
        }
        if *self.epoch.borrow() == target {
            self.inertia.borrow_mut().active = false;
            self.update_state(None);
        } else {
            self.state.set(FiberState::Unloading);
            self.start_unload();
            self.update_state(Some(FiberState::Unloading));
        }
    }

    async fn unload(self: Rc<Self>) {
        let disposers = self.disposables.take();
        for disposer in disposers.into_iter().rev() {
            if let Err(reason) = catch_disposer(disposer).await {
                self.log_error(&reason);
            }
        }
        if *self.epoch.borrow() == Epoch::Inactive {
            self.inertia.borrow_mut().active = false;
            self.update_state(None);
        } else {
            self.state.set(FiberState::Loading);
            self.start_reload();
            self.update_state(Some(FiberState::Loading));
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

    fn update_state(self: &Rc<Self>, explicit: Option<FiberState>) {
        let old = self.state.get();
        let new = explicit.unwrap_or_else(|| self.get_state());
        self.state.set(new);
        if old == new {
            return;
        }
        // Notify consumers when crossing the ACTIVE boundary (B8 refines the
        // reflect-store notify semantics).
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

    fn log_error(&self, reason: &dyn fmt::Display) {
        if let Some(logger) = self.ctx.get_service::<crate::LoggerService>("logger") {
            logger.error(reason);
        }
    }

    /// Executes an effect callback and collects disposers into the handle.
    fn run_effect<F>(
        &self,
        execute: F,
        handle: &Rc<EffectHandle>,
    ) -> Result<Option<BoxFuture<'static, ()>>, Box<dyn Error>>
    where
        F: FnOnce() -> Effect,
    {
        match execute() {
            Effect::None => Ok(None),
            Effect::Disposer(disposer) => {
                handle.collect(disposer);
                Ok(None)
            }
            Effect::Async(future) => {
                let handle = handle.clone();
                Ok(Some(Box::pin(async move {
                    let disposer = future.await;
                    handle.collect(disposer);
                })))
            }
            Effect::Iterable(disposers) => {
                for disposer in disposers {
                    handle.collect(disposer);
                }
                Ok(None)
            }
            Effect::AsyncIterable(stream) => {
                let handle = handle.clone();
                let mut stream = stream;
                Ok(Some(Box::pin(async move {
                    poll_fn(|cx| {
                        loop {
                            if handle.is_disposed() {
                                return Poll::Ready(());
                            }
                            match stream.as_mut().poll_next(cx) {
                                Poll::Ready(Some(disposer)) => handle.collect(disposer),
                                Poll::Ready(None) => return Poll::Ready(()),
                                Poll::Pending => return Poll::Pending,
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
    ) -> Result<Option<BoxFuture<'static, ()>>, Box<dyn Error>> {
        let empty: Rc<dyn Any> = Rc::new(());
        let config = config.as_ref().unwrap_or(&empty);
        let target = self.epoch.borrow().clone();
        match callback(ctx, config) {
            Effect::None => Ok(None),
            Effect::Disposer(disposer) => {
                self.disposables.borrow_mut().push(disposer);
                Ok(None)
            }
            Effect::Async(future) => {
                let this = self.clone();
                Ok(Some(Box::pin(async move {
                    eprintln!(
                        "[fiber {}] awaiting async apply",
                        this.uid.get().unwrap_or(0)
                    );
                    let disposer = future.await;
                    eprintln!("[fiber {}] async apply done", this.uid.get().unwrap_or(0));
                    this.disposables.borrow_mut().push(disposer);
                })))
            }
            Effect::Iterable(disposers) => {
                let mut list = self.disposables.borrow_mut();
                for disposer in disposers {
                    list.push(disposer);
                }
                Ok(None)
            }
            Effect::AsyncIterable(stream) => {
                let this = self.clone();
                let mut stream = stream;
                Ok(Some(Box::pin(async move {
                    poll_fn(|cx| {
                        loop {
                            if *this.epoch.borrow() != target {
                                return Poll::Ready(());
                            }
                            match stream.as_mut().poll_next(cx) {
                                Poll::Ready(Some(disposer)) => {
                                    this.disposables.borrow_mut().push(disposer);
                                }
                                Poll::Ready(None) => return Poll::Ready(()),
                                Poll::Pending => return Poll::Pending,
                            }
                        }
                    })
                    .await
                })))
            }
            Effect::Error(reason) => Err(reason),
        }
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

async fn catch_disposer(disposer: Disposer) -> Result<(), Box<dyn Error>> {
    disposer().await
}

/// Creates a disposer from a sync closure (public convenience helper).
pub fn disposer<F>(f: F) -> Disposer
where
    F: FnOnce() + 'static,
{
    sync_disposer(f)
}
