//! Service contract and effect types shared by the core runtime.

use std::any::Any;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context as TaskContext, Poll};

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
    /// A promise resolving to a disposer.
    Async(BoxFuture<'static, Disposer>),
    /// A collection of disposers (iterable).
    Iterable(Vec<Disposer>),
    /// An asynchronous stream of disposers (async iterable).
    AsyncIterable(Pin<Box<dyn AsyncDisposerStream>>),
    /// The callback threw an error (mirrors a throwing apply/effect).
    Error(Box<dyn Error>),
}

/// An asynchronous iterator over disposers.
pub trait AsyncDisposerStream {
    /// Polls the stream for the next disposer.
    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Disposer>>;
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
