//! Event dispatch service: the five dispatch modes (`emit`, `parallel`,
//! `serial`, `bail`, `waterfall`), listener lifecycle and filters.
//!
//! Listeners may be synchronous or asynchronous. `parallel` starts every
//! listener before awaiting any of them, `serial` awaits them in order and
//! `waterfall` awaits the whole chain; `emit` and `bail` stay synchronous
//! (as in Cordis) and consume only the first poll of a listener future.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::rc::{Rc, Weak};
use std::task::{Context as TaskContext, Poll, Waker};

use crate::context::{Context, ContextInner};
use crate::fiber::{CordisError, EffectHandle, Fiber};
use crate::service::{BoxFuture, Effect, Service, sync_disposer};

/// A single event listener.
///
/// Produces a boxed future that resolves to `Ok(Some(value))` for a truthy
/// result (used by `serial`/`bail`/`waterfall`) or `Err` to propagate an
/// error (used by all modes). As in Cordis, listeners may be synchronous or
/// asynchronous; the dispatch mode decides whether the future is awaited.
pub type EventCallback = Rc<
    dyn Fn(&[Rc<dyn Any>]) -> BoxFuture<'static, Result<Option<Rc<dyn Any>>, Box<dyn Error>>>
        + 'static,
>;

/// Adapts a plain listener closure into an [`EventCallback`].
///
/// The listener is synchronous: it runs to completion the first time the
/// returned future is polled.
pub fn event_listener<F>(f: F) -> EventCallback
where
    F: Fn(&[Rc<dyn Any>]) + 'static,
{
    let f = Rc::new(f);
    Rc::new(move |args: &[Rc<dyn Any>]| {
        let args = args.to_vec();
        let f = f.clone();
        Box::pin(async move {
            f(&args);
            Ok(None)
        })
    })
}

/// Adapts a synchronous listener returning a `Result` into an
/// [`EventCallback`].
///
/// Equivalent to the previous synchronous `EventCallback` signature: the
/// listener runs to completion on the first poll, so `emit` and `bail` can
/// consume its result without awaiting.
pub fn event_callback<F>(f: F) -> EventCallback
where
    F: Fn(&[Rc<dyn Any>]) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> + 'static,
{
    let f = Rc::new(f);
    Rc::new(move |args: &[Rc<dyn Any>]| {
        let args = args.to_vec();
        let f = f.clone();
        Box::pin(async move { f(&args) })
    })
}

/// Adapts an asynchronous listener into an [`EventCallback`].
///
/// The future is created when the callback is invoked and first polled by
/// the dispatch mode (e.g. `join_all` in `parallel`, sequential awaits in
/// `serial`).
pub fn event_listener_async<F, Fut>(f: F) -> EventCallback
where
    F: Fn(Vec<Rc<dyn Any>>) -> Fut + 'static,
    Fut: Future<Output = Result<Option<Rc<dyn Any>>, Box<dyn Error>>> + 'static,
{
    let f = Rc::new(f);
    Rc::new(move |args: &[Rc<dyn Any>]| {
        let f = f.clone();
        Box::pin(f(args.to_vec()))
    })
}

/// The `next` function handed to waterfall listeners.
///
/// Resolves to the downstream listener's result; the chain is driven by
/// whoever awaits the returned future.
pub type WaterfallNext = Rc<dyn Fn() -> BoxFuture<'static, ListenerResult>>;

/// The result type of a listener invocation.
type ListenerResult = Result<Option<Rc<dyn Any>>, Box<dyn Error>>;

/// Polls a listener future once with a no-op waker.
///
/// Synchronous listeners always resolve on the first poll; an asynchronous
/// listener reports `Pending` here and must be awaited by the dispatch mode.
pub(crate) fn poll_once(future: &mut BoxFuture<'static, ListenerResult>) -> Poll<ListenerResult> {
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);
    future.as_mut().poll(&mut cx)
}

/// Boxes a [`WaterfallNext`] so it can travel inside `Rc<dyn Any>` args.
#[derive(Clone)]
pub struct AnyNext(pub WaterfallNext);

/// Event name → listeners table.
type HookTable = Rc<RefCell<HashMap<String, Vec<Hook>>>>;

/// A registered hook.
#[derive(Clone)]
struct Hook {
    ctx: Weak<ContextInner>,
    fiber: Weak<Fiber>,
    global: bool,
    callback: EventCallback,
    filter: Option<ListenerFilter>,
}

/// Options accepted by [`EventsService::on`].
#[derive(Clone, Debug, Default)]
pub struct EventOptions {
    /// Whether to prepend the listener.
    pub prepend: bool,
    /// Whether the listener ignores filters.
    pub global: bool,
}

/// A filter closure attached to a listener registration.
pub type ListenerFilter = Rc<dyn Fn(&dyn EventFilter) -> bool>;

/// A filter attached to a dispatch call (`thisArg` in the TS reference).
pub trait EventFilter: Any {
    /// Downcasts the filter to its concrete type.
    fn as_any(&self) -> &dyn Any;
    /// Returns whether a hook registered on `hook_ctx` should run.
    fn filter(&self, hook_ctx: &Context) -> bool;
}

/// Aggregated errors of a `parallel` dispatch (mirrors `AggregateError`).
#[derive(Debug)]
pub struct ParallelError {
    /// All rejected listener errors.
    pub errors: Vec<Box<dyn Error>>,
}

impl fmt::Display for ParallelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} listener(s) failed: {:?}",
            self.errors.len(),
            self.errors
                .iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
        )
    }
}

impl Error for ParallelError {}

/// Event dispatch service, available on every context as `ctx.events`.
pub struct EventsService {
    hooks: HookTable,
}

impl Default for EventsService {
    fn default() -> Self {
        Self {
            hooks: Rc::new(RefCell::new(HashMap::new())),
        }
    }
}

impl Service for EventsService {
    const NAME: &'static str = "events";
}

impl EventsService {
    /// Registers a listener bound to the fiber of `ctx`.
    ///
    /// Mirrors `ctx.on()`: the registration is an effect of `ctx`'s fiber,
    /// so it is removed when the fiber unloads. `internal/update` hooks are
    /// stored on the fiber itself (mirrors the `EventsService` constructor).
    pub fn on(
        &self,
        ctx: &Context,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        ctx.fiber().assert_active()?;
        if event == "internal/update" && !options.global {
            return ctx
                .fiber()
                .register_internal_hook(event, callback, options.prepend);
        }
        let event = event.to_string();
        let effect_label = format!("ctx.on({event:?})");
        self.on_impl(ctx, event, callback, options, None, &effect_label)
    }

    /// The service-level `internal/update` hooks (global ones only; non-global
    /// hooks are stored on fibers).
    pub(crate) fn global_internal_update_hooks(&self) -> Vec<EventCallback> {
        self.hooks
            .borrow()
            .get("internal/update")
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|hook| hook.callback)
            .collect()
    }

    /// Registers a listener with an attached filter (Rust equivalent of the
    /// TS pattern of extending a context with a filter function).
    pub fn on_filtered(
        &self,
        ctx: &Context,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
        filter: ListenerFilter,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        ctx.fiber().assert_active()?;
        if event == "internal/update" && !options.global {
            return ctx
                .fiber()
                .register_internal_hook(event, callback, options.prepend);
        }
        let event = event.to_string();
        let effect_label = format!("ctx.on({event:?})");
        self.on_impl(ctx, event, callback, options, Some(filter), &effect_label)
    }

    fn on_impl(
        &self,
        ctx: &Context,
        event: String,
        callback: EventCallback,
        options: EventOptions,
        filter: Option<ListenerFilter>,
        effect_label: &str,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        let hooks = Rc::clone(&self.hooks);
        ctx.fiber().effect(
            move || {
                let hook = Hook {
                    ctx: Rc::downgrade(&ctx.inner),
                    fiber: Rc::downgrade(ctx.fiber()),
                    global: options.global,
                    callback: callback.clone(),
                    filter: filter.clone(),
                };
                let mut hooks_borrow = hooks.borrow_mut();
                let list = hooks_borrow.entry(event.clone()).or_default();
                if options.prepend {
                    list.insert(0, hook);
                } else {
                    list.push(hook);
                }
                drop(hooks_borrow);
                let hooks = Rc::clone(&hooks);
                let event = event.clone();
                Effect::Disposer(sync_disposer(move || {
                    if let Some(list) = hooks.borrow_mut().get_mut(&event)
                        && let Some(position) = list
                            .iter()
                            .position(|hook| Rc::ptr_eq(&hook.callback, &callback))
                    {
                        list.remove(position);
                    }
                }))
            },
            effect_label,
        )
    }

    /// Registers a listener that runs at most once.
    pub fn once(
        &self,
        ctx: &Context,
        event: &str,
        callback: EventCallback,
        options: EventOptions,
    ) -> Result<Rc<EffectHandle>, CordisError> {
        let event = event.to_string();
        let callback = callback.clone();
        let called = Rc::new(Cell::new(false));
        let wrapper: EventCallback = Rc::new(move |args: &[Rc<dyn Any>]| {
            if called.replace(true) {
                return Box::pin(async { Ok(None) });
            }
            callback(args)
        });
        self.on(ctx, &event, wrapper, options)
    }

    /// Emits synchronously; the first listener error propagates.
    pub fn emit(&self, ctx: &Context, event: &str, args: &[Rc<dyn Any>]) {
        self.emit_with(ctx, event, args, None)
    }

    /// Emits with a filter (`thisArg` in the TS reference).
    ///
    /// Synchronous listeners run immediately and the first error panics
    /// (mirrors the JS `emit`). An asynchronous listener is started and its
    /// continuation runs in the background, like the ignored promise of a JS
    /// async listener; this requires a current-thread runtime or `LocalSet`
    /// (the runtime model Cordis assumes), and its completion is logged.
    pub fn emit_with(
        &self,
        ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) {
        let callbacks = self.resolve("emit", event, args, this_arg, ctx);
        self.emit_callbacks(ctx, callbacks, args);
    }

    /// Runs all listeners and awaits them together, aggregating errors
    /// (mirrors `Promise.allSettled` + `AggregateError`).
    ///
    /// As in Cordis, every listener is started before any completion is
    /// awaited, so asynchronous listeners overlap at their `await` points.
    pub async fn parallel(
        &self,
        ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<(), ParallelError> {
        let callbacks = self.resolve("emit", event, args, this_arg, ctx);
        self.parallel_resolved(event, callbacks, args).await
    }

    /// Awaits an already-resolved listener snapshot together, aggregating
    /// errors (mirrors `Promise.allSettled` + `AggregateError`). Every
    /// listener is started before any completion is awaited.
    pub async fn parallel_resolved(
        &self,
        _event: &str,
        callbacks: Vec<EventCallback>,
        args: &[Rc<dyn Any>],
    ) -> Result<(), ParallelError> {
        let results =
            futures_util::future::join_all(callbacks.into_iter().map(|callback| callback(args)))
                .await;
        let mut errors = Vec::new();
        for result in results {
            if let Err(error) = result {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ParallelError { errors })
        }
    }

    /// Runs listeners sequentially, awaiting each in order and
    /// short-circuiting on the first truthy result; errors propagate.
    pub async fn serial(
        &self,
        ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
        let callbacks = self.resolve("serial", event, args, this_arg, ctx);
        for callback in callbacks {
            if let Some(result) = callback(args).await? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Runs listeners synchronously, short-circuiting on the first truthy
    /// result; errors propagate.
    pub fn bail(
        &self,
        ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
        let callbacks = self.resolve("bail", event, args, this_arg, ctx);
        for callback in callbacks {
            let mut future = callback(args);
            match poll_once(&mut future) {
                Poll::Ready(result) => {
                    if let Some(result) = result? {
                        return Ok(Some(result));
                    }
                }
                Poll::Pending => {
                    return Err("bail does not support asynchronous listeners (use serial)".into());
                }
            }
        }
        Ok(None)
    }

    /// Runs listeners in a waterfall chain; each listener receives `args`
    /// plus a `next` function, and the last call falls back to `tail`.
    ///
    /// The chain is awaited as a whole: listeners may be asynchronous and
    /// `next()` returns a future (mirrors the JS waterfall, where async
    /// listeners make the whole chain awaitable). An optional `this_arg`
    /// filters listeners by scope (mirrors the JS `thisArg`).
    pub fn waterfall(
        &self,
        ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
        tail: WaterfallNext,
    ) -> BoxFuture<'static, ListenerResult> {
        let mut callbacks = self
            .resolve("waterfall", event, args, this_arg, ctx)
            .into_iter()
            .collect::<Vec<_>>();
        let first = if callbacks.is_empty() {
            None
        } else {
            Some(callbacks.remove(0))
        };
        let callbacks = Rc::new(RefCell::new(callbacks));
        let args = args.to_vec();
        let args_for_inner = args.clone();
        Box::pin(async move {
            match first {
                Some(callback) => {
                    let inner: WaterfallNext = Rc::new(move || {
                        run_waterfall_step(callbacks.clone(), args_for_inner.clone(), tail.clone())
                    });
                    let mut next_args = args.clone();
                    let next_any: Rc<dyn Any> = Rc::new(AnyNext(inner));
                    next_args.push(next_any);
                    callback(&next_args).await
                }
                None => tail().await,
            }
        })
    }

    /// Dispatches to listeners with `emit` semantics.
    fn emit_callbacks(&self, ctx: &Context, callbacks: Vec<EventCallback>, args: &[Rc<dyn Any>]) {
        for callback in callbacks {
            let mut future = callback(args);
            match poll_once(&mut future) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(error)) => panic!("emit listener failed: {error}"),
                Poll::Pending => {
                    let logger = ctx.logger();
                    tokio::task::spawn_local(async move {
                        if let Err(error) = future.await {
                            logger.error(format!("emit listener failed asynchronously: {error}"));
                        }
                    });
                }
            }
        }
    }

    /// Resolves the listener snapshot for one dispatch without invoking the
    /// listeners. Fires `internal/dispatch` hooks and applies scope filters
    /// exactly like the other dispatch modes, so a caller can capture the
    /// snapshot at one point and invoke it later (the JS `_resolve` split).
    pub fn resolve_callbacks(
        &self,
        ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Vec<EventCallback> {
        self.resolve("emit", event, args, this_arg, ctx)
    }

    /// Invokes an already-resolved listener snapshot with per-listener
    /// containment: every failure is logged, never propagated, so a caller's
    /// committed mutation cannot fail because an observer threw.
    pub fn emit_resolved_contained(
        &self,
        ctx: &Context,
        event: &str,
        callbacks: Vec<EventCallback>,
        args: &[Rc<dyn Any>],
    ) {
        let event = event.to_string();
        for callback in callbacks {
            let mut future = callback(args);
            match poll_once(&mut future) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(error)) => {
                    let logger = ctx.logger();
                    logger.warn(format!("{event} listener threw: {error}"));
                }
                Poll::Pending => {
                    let logger = ctx.logger();
                    let event = event.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(error) = future.await {
                            logger.warn(format!("{event} listener rejected: {error}"));
                        }
                    });
                }
            }
        }
    }

    /// Invokes an already-resolved listener snapshot with veto semantics: the
    /// first synchronous failure propagates and later listeners do not run;
    /// asynchronous rejections are logged and cannot veto this synchronous
    /// boundary. Used by publication points such as `session/created`.
    pub fn emit_resolved_veto(
        &self,
        ctx: &Context,
        event: &str,
        callbacks: Vec<EventCallback>,
        args: &[Rc<dyn Any>],
    ) -> Result<(), Box<dyn Error>> {
        let event = event.to_string();
        for callback in callbacks {
            let mut future = callback(args);
            match poll_once(&mut future) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(error)) => return Err(error),
                Poll::Pending => {
                    let logger = ctx.logger();
                    let event = event.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(error) = future.await {
                            logger.warn(format!("{event} listener rejected: {error}"));
                        }
                    });
                }
            }
        }
        Ok(())
    }

    fn resolve(
        &self,
        mode: &'static str,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
        ctx: &Context,
    ) -> Vec<EventCallback> {
        // `internal/dispatch` extension point: before a non-internal event
        // is dispatched, notify dispatch hooks with the mode, event name and
        // payload args. Internal events are excluded to avoid recursion,
        // mirroring the TS `_resolve` guard.
        let has_dispatch_hooks = self
            .hooks
            .borrow()
            .get("internal/dispatch")
            .is_some_and(|hooks| !hooks.is_empty());
        if !event.starts_with("internal/") && has_dispatch_hooks {
            let dispatch_args: Vec<Rc<dyn Any>> = vec![
                Rc::new(mode.to_string()),
                Rc::new(event.to_string()),
                Rc::new(args.to_vec()),
            ];
            let callbacks = self.resolve("emit", "internal/dispatch", &dispatch_args, None, ctx);
            self.emit_callbacks(ctx, callbacks, &dispatch_args);
        }
        let listeners = self.hooks.borrow().get(event).cloned().unwrap_or_default();
        listeners
            .into_iter()
            .filter(|hook| {
                if hook.global {
                    return true;
                }
                let Some(this_arg) = this_arg else {
                    return true;
                };
                let Some(hook_ctx) = hook.ctx.upgrade() else {
                    return false;
                };
                let Some(hook_fiber) = hook.fiber.upgrade() else {
                    return false;
                };
                let hook_context = Context {
                    inner: hook_ctx,
                    fiber: hook_fiber,
                };
                match &hook.filter {
                    Some(filter) => filter(this_arg),
                    None => this_arg.filter(&hook_context),
                }
            })
            .map(|hook| hook.callback)
            .collect()
    }
}

pub(crate) fn run_waterfall_step(
    callbacks: Rc<RefCell<Vec<EventCallback>>>,
    args: Vec<Rc<dyn Any>>,
    tail: WaterfallNext,
) -> BoxFuture<'static, ListenerResult> {
    Box::pin(async move {
        let next_callback = callbacks.borrow_mut().first().cloned();
        if let Some(callback) = next_callback {
            callbacks.borrow_mut().remove(0);
            let args_for_inner = args.clone();
            let inner: WaterfallNext = Rc::new(move || {
                run_waterfall_step(callbacks.clone(), args_for_inner.clone(), tail.clone())
            });
            let mut next_args = args.clone();
            let next_any: Rc<dyn Any> = Rc::new(AnyNext(inner));
            next_args.push(next_any);
            callback(&next_args).await
        } else {
            tail().await
        }
    })
}
