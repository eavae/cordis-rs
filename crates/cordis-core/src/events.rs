//! Event dispatch service: the five dispatch modes (`emit`, `parallel`,
//! `serial`, `bail`, `waterfall`), listener lifecycle and filters.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::rc::{Rc, Weak};

use crate::context::{Context, ContextInner};
use crate::fiber::{CordisError, EffectHandle, Fiber};
use crate::service::{Effect, Service, sync_disposer};

/// A single event listener.
///
/// Returns `Ok(Some(value))` for a truthy result (used by `serial`/`bail`/
/// `waterfall`) or `Err` to propagate an error (used by all modes).
pub type EventCallback = Rc<dyn Fn(&[Rc<dyn Any>]) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>>>;

/// Adapts a plain listener closure into an [`EventCallback`].
pub fn event_listener<F>(f: F) -> EventCallback
where
    F: Fn(&[Rc<dyn Any>]) + 'static,
{
    Rc::new(move |args| {
        f(args);
        Ok(None)
    })
}

/// The `next` function handed to waterfall listeners.
pub type WaterfallNext = Rc<dyn Fn() -> Option<Rc<dyn Any>>>;

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
        EventsService {
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
        let wrapper: EventCallback = {
            let called = called.clone();
            Rc::new(
                move |args: &[Rc<dyn Any>]| -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
                    if called.replace(true) {
                        return Ok(None);
                    }
                    callback(args)
                },
            )
        };
        self.on(ctx, &event, wrapper, options)
    }

    /// Emits synchronously; the first listener error propagates.
    pub fn emit(&self, ctx: &Context, event: &str, args: &[Rc<dyn Any>]) {
        self.emit_with(ctx, event, args, None)
    }

    /// Emits with a filter (`thisArg` in the TS reference).
    pub fn emit_with(
        &self,
        _ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) {
        let callbacks = self.resolve("emit", event, args, this_arg);
        for callback in callbacks {
            callback(args).expect("emit listener failed");
        }
    }

    /// Runs listeners concurrently, aggregating errors (mirrors
    /// `Promise.allSettled` + `AggregateError`).
    pub async fn parallel(
        &self,
        _ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<(), ParallelError> {
        let callbacks = self.resolve("emit", event, args, this_arg);
        let mut errors = Vec::new();
        for callback in callbacks {
            if let Err(error) = callback(args) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ParallelError { errors })
        }
    }

    /// Runs listeners sequentially, short-circuiting on the first truthy
    /// result; errors propagate.
    pub async fn serial(
        &self,
        _ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
        let callbacks = self.resolve("serial", event, args, this_arg);
        for callback in callbacks {
            if let Some(result) = callback(args)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Runs listeners synchronously, short-circuiting on the first truthy
    /// result; errors propagate.
    pub fn bail(
        &self,
        _ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
        let callbacks = self.resolve("bail", event, args, this_arg);
        for callback in callbacks {
            if let Some(result) = callback(args)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    /// Runs listeners in a waterfall chain; each listener receives `args`
    /// plus a `next` function, and the last call falls back to `tail`.
    pub fn waterfall(
        &self,
        _ctx: &Context,
        event: &str,
        args: &[Rc<dyn Any>],
        tail: WaterfallNext,
    ) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
        let mut callbacks = self
            .resolve("waterfall", event, args, None)
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
        match first {
            Some(callback) => {
                let inner: WaterfallNext = Rc::new(move || {
                    run_waterfall_step(callbacks.clone(), args_for_inner.clone(), tail.clone())
                        .ok()
                        .flatten()
                });
                let mut next_args = args.clone();
                let next_any: Rc<dyn Any> = Rc::new(AnyNext(inner));
                next_args.push(next_any);
                callback(&next_args)
            }
            None => Ok(tail()),
        }
    }

    fn resolve(
        &self,
        mode: &'static str,
        event: &str,
        args: &[Rc<dyn Any>],
        this_arg: Option<&dyn EventFilter>,
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
            let callbacks = self.resolve("emit", "internal/dispatch", &dispatch_args, None);
            for callback in callbacks {
                callback(&dispatch_args).expect("internal/dispatch listener failed");
            }
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
) -> Result<Option<Rc<dyn Any>>, Box<dyn Error>> {
    let next_callback = callbacks.borrow_mut().first().cloned();
    if let Some(callback) = next_callback {
        callbacks.borrow_mut().remove(0);
        let args_for_inner = args.clone();
        let inner = Rc::new(move || {
            run_waterfall_step(callbacks.clone(), args_for_inner.clone(), tail.clone())
                .ok()
                .flatten()
        });
        let mut next_args = args.clone();
        let next_any: Rc<dyn Any> = Rc::new(AnyNext(inner));
        next_args.push(next_any);
        callback(&next_args)
    } else {
        Ok(tail())
    }
}
