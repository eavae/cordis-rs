//! Service contract and effect types shared by the core runtime.

use std::any::Any;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};

use crate::fiber::EffectHandle;

/// A boxed future returned by async APIs.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A boxed error that can travel between threads.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// A disposer returned by side-effect registration APIs.
///
/// Calling it removes the registered side effect. Idempotence, ordering and
/// async cleanup are handled by the effect executor ([`Fiber::effect`](crate::Fiber::effect)).
pub type Disposer = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), BoxError>> + Send>;

/// Wraps a plain synchronous closure into a [`Disposer`].
pub fn sync_disposer<F>(f: F) -> Disposer
where
    F: FnOnce() + Send + 'static,
{
    Box::new(move || {
        Box::pin(async move {
            f();
            Ok(())
        })
    })
}

/// Wraps an asynchronous closure into a [`Disposer`].
pub fn async_disposer<F, Fut>(f: F) -> Disposer
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
{
    Box::new(move || Box::pin(f()))
}

/// The value returned by plugin entries and effect callbacks.
///
/// Mirrors the four accepted effect shapes in the TS reference (`Disposable`,
/// `Promise<Disposable>`, `Iterable<Disposable>`, `AsyncIterable<Disposable>`);
/// the `None` variant mirrors `null`/`undefined`.
pub enum Effect {
    /// No effect (`null` / `undefined`).
    None,
    /// A synchronous disposer.
    Disposer(Disposer),
    /// A nested effect (yielded or returned as a whole).
    Nested(Arc<EffectHandle>),
    /// A promise resolving to a disposer (or failing).
    Async(BoxFuture<'static, Result<Disposer, BoxError>>),
    /// A collection of effect items (iterable); an `Err` item aborts the
    /// collection and propagates the error (mirrors a generator that throws).
    Iterable(Vec<Result<EffectItem, BoxError>>),
    /// An asynchronous stream of disposers (async iterable).
    AsyncIterable(Pin<Box<dyn AsyncDisposerStream + Send>>),
    /// The callback threw an error (mirrors a throwing apply/effect).
    Error(BoxError),
}

/// One item yielded by an iterable effect.
pub enum EffectItem {
    /// A plain disposer.
    Disposer(Disposer),
    /// A nested effect handle.
    Nested(Arc<EffectHandle>),
}

/// Cleanup returned by a typed plugin closure ([`plugin_sync`](crate::plugin_sync) /
/// [`plugin_async`](crate::plugin_async)).
///
/// Carries zero or more disposers that run when the plugin's fiber unloads,
/// in reverse registration order. The synchronous adapter converts it into
/// an [`Effect`]; the asynchronous adapter awaits the closure and delivers a
/// combined disposer.
#[derive(Default)]
pub struct PluginOutput {
    disposers: Vec<Disposer>,
}

impl PluginOutput {
    /// No additional cleanup (effects registered through `ctx` are still
    /// owned by the fiber).
    pub fn none() -> Self {
        Self::default()
    }

    /// One fallible synchronous disposer.
    pub fn disposer<F>(dispose: F) -> Self
    where
        F: FnOnce() -> Result<(), BoxError> + Send + 'static,
    {
        Self::default().with_disposer(dispose)
    }

    /// One infallible synchronous disposer.
    pub fn infallible<F>(dispose: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::default().with_disposer(move || {
            dispose();
            Ok(())
        })
    }

    /// One asynchronous disposer.
    pub fn async_disposer<F, Fut>(dispose: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), BoxError>> + Send + 'static,
    {
        let mut output = Self::default();
        output.disposers.push(Box::new(move || Box::pin(dispose())));
        output
    }

    /// Appends another disposer.
    pub fn with_disposer<F>(mut self, dispose: F) -> Self
    where
        F: FnOnce() -> Result<(), BoxError> + Send + 'static,
    {
        self.disposers
            .push(Box::new(move || Box::pin(async move { dispose() })));
        self
    }

    /// The number of disposers.
    pub fn len(&self) -> usize {
        self.disposers.len()
    }

    /// Whether any disposer is present.
    pub fn is_empty(&self) -> bool {
        self.disposers.is_empty()
    }

    pub(crate) fn into_effect(self) -> Effect {
        let mut disposers = self.disposers;
        match disposers.len() {
            0 => Effect::None,
            1 => Effect::Disposer(disposers.pop().expect("len checked")),
            _ => Effect::Iterable(
                disposers
                    .into_iter()
                    .map(|disposer| Ok(EffectItem::Disposer(disposer)))
                    .collect(),
            ),
        }
    }

    pub(crate) fn into_disposer(self) -> Disposer {
        let mut disposers = self.disposers;
        if disposers.len() == 1 {
            return disposers.pop().expect("len checked");
        }
        Box::new(move || {
            Box::pin(async move {
                for disposer in disposers {
                    disposer().await?;
                }
                Ok(())
            })
        })
    }
}

/// An asynchronous iterator over disposers.
pub trait AsyncDisposerStream {
    /// Polls the stream for the next disposer.
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Disposer, BoxError>>>;
}

/// The apply callback of a plugin (or inject callback).
pub type ApplyFn =
    Arc<dyn Fn(&crate::Context, &Arc<dyn Any + Send + Sync>) -> Effect + Send + Sync>;

/// A Cordis service.
///
/// Services are registered into a [`Context`](crate::Context)'s store under
/// [`Service::NAME`] and can be retrieved through the typed path
/// `Context::get::<S>()` or the dynamic path `Context::get_str(name)`.
///
/// The full service contract (config, check, resolve_config) extends this;
/// the minimal form only carries the stable service name.
pub trait Service: Any + Send + Sync {
    /// Stable service name used for dynamic access (`ctx.get_str(name)`).
    const NAME: &'static str;

    /// Whether the service is currently usable.
    ///
    /// Mirrors the TS `Service.check`; injectors whose dependencies fail the
    /// check stay `PENDING`.
    fn check(&self, _ctx: &crate::Context) -> bool {
        true
    }

    /// Callable-service invocation (mirrors `[Service.invoke]`).
    ///
    /// The default returns `None` (not callable).
    ///
    /// The [`ShadowContext`](crate::ShadowContext) carries both scopes the
    /// TS proxy provides through `this.ctx`: dependency reads (e.g.
    /// [`get_str`](crate::ShadowContext::get_str)) resolve through the
    /// service's own shadow, while everything else (intercept, fiber,
    /// effects, plugins) follows the caller's context.
    fn invoke(
        &self,
        _ctx: &crate::ShadowContext,
        _init: Option<&Arc<dyn Any + Send + Sync>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
}

/// Marker trait for service config values.
///
/// The full `Config` trait (serde-backed) supersedes this. The current
/// minimal contract only requires a merge operation so that intercept
/// chains can be resolved (mirroring `Object.assign` in the TS reference
/// implementation).
pub trait Config: Any + Clone + Default {
    /// Merges `other` into `self`; fields of `other` win.
    ///
    /// Mirrors the `Object.assign` semantics used by `Service::resolveConfig`
    /// in the TS reference.
    fn merge(&self, other: &Self) -> Self;
}
