//! Service contract and effect types shared by the core runtime.

use std::any::Any;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Poll};

use crate::fiber::EffectHandle;

/// A boxed future returned by async APIs.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A disposer returned by side-effect registration APIs.
///
/// Calling it removes the registered side effect. Idempotence, ordering and
/// async cleanup are handled by the effect executor ([`Fiber::effect`](crate::Fiber::effect)).
pub type Disposer = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), Box<dyn Error>>>>;

/// Wraps a plain synchronous closure into a [`Disposer`].
pub fn sync_disposer<F>(f: F) -> Disposer
where
    F: FnOnce() + 'static,
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
    F: FnOnce() -> Fut + 'static,
    Fut: Future<Output = Result<(), Box<dyn Error>>> + 'static,
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
    Nested(Rc<EffectHandle>),
    /// A promise resolving to a disposer (or failing).
    Async(BoxFuture<'static, Result<Disposer, Box<dyn Error>>>),
    /// A collection of effect items (iterable); an `Err` item aborts the
    /// collection and propagates the error (mirrors a generator that throws).
    Iterable(Vec<Result<EffectItem, Box<dyn Error>>>),
    /// An asynchronous stream of disposers (async iterable).
    AsyncIterable(Pin<Box<dyn AsyncDisposerStream>>),
    /// The callback threw an error (mirrors a throwing apply/effect).
    Error(Box<dyn Error>),
}

/// One item yielded by an iterable effect.
pub enum EffectItem {
    /// A plain disposer.
    Disposer(Disposer),
    /// A nested effect handle.
    Nested(Rc<EffectHandle>),
}

/// An asynchronous iterator over disposers.
pub trait AsyncDisposerStream {
    /// Polls the stream for the next disposer.
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Disposer, Box<dyn Error>>>>;
}

/// The apply callback of a plugin (or inject callback).
pub type ApplyFn = Rc<dyn Fn(&crate::Context, &Rc<dyn Any>) -> Effect>;

/// A Cordis service.
///
/// Services are registered into a [`Context`](crate::Context)'s store under
/// [`Service::NAME`] and can be retrieved through the typed path
/// `Context::get::<S>()` or the dynamic path `Context::get_str(name)`.
///
/// The full service contract (config, check, resolve_config) is defined in
/// story card B6; this minimal form only carries the stable service name.
pub trait Service: Any {
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
    fn invoke(
        &self,
        _ctx: &crate::Context,
        _init: Option<&std::rc::Rc<dyn Any>>,
    ) -> Option<std::rc::Rc<dyn Any>> {
        None
    }
}

/// Marker trait for service config values.
///
/// Story card B6 replaces this with the full `Config` trait (serde-backed).
/// The current minimal contract only requires a merge operation so that
/// intercept chains can be resolved (mirroring `Object.assign` in the TS
/// reference implementation).
pub trait Config: Any + Clone + Default {
    /// Merges `other` into `self`; fields of `other` win.
    ///
    /// Mirrors the `Object.assign` semantics used by `Service::resolveConfig`
    /// in the TS reference.
    fn merge(&self, other: &Self) -> Self;
}
