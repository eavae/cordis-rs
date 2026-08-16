//! Service contract shared by all Cordis services.

use std::any::Any;

/// A disposer returned by side-effect registration APIs.
///
/// Calling it once removes the registered side effect. Idempotence, ordering
/// and async cleanup are handled by the effect executor (story card B3).
pub type Disposer = Box<dyn FnOnce()>;

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
