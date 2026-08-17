//! Reflect service: the full dynamic access surface.
//!
//! The property table, accessors and mixins build on top of this service,
//! which completes `get`/`set`/`has`/`accessor`.

use std::any::Any;
use std::rc::Rc;

use crate::context::{Context, MixinGet, MixinSet};
use crate::fiber::EffectHandle;
use crate::service::Service;

/// Reflect service, available on every context as `ctx.reflect`.
///
/// Unlike the TS reference (where methods close over the root context via
/// `this`), the Rust methods take the calling context explicitly.
#[derive(Debug)]
pub struct ReflectService;

impl Service for ReflectService {
    const NAME: &'static str = "reflect";
}

impl ReflectService {
    /// Reads a service value (mirrors `reflect.get(name, strict)`).
    ///
    /// `strict` requires the provider fiber to be `ACTIVE`; non-strict reads
    /// return the value of any registered entry, even while the provider is
    /// unloading or failed.
    pub fn get(&self, ctx: &Context, name: &str, strict: bool) -> Option<Rc<dyn Any>> {
        if strict {
            ctx.get_str(name)
        } else {
            ctx.get_str_non_strict(name)
        }
    }

    /// Writes a service value (mirrors `reflect.set(name, value)`).
    ///
    /// Enforces ownership: only the providing fiber may set the value, and
    /// injectors are notified after the update.
    pub fn set(&self, ctx: &Context, name: &str, value: Rc<dyn Any>) -> Result<(), String> {
        ctx.set_str(name, value)
    }

    /// Whether `name` resolves as a property (mirrors the `in` operator).
    pub fn has(&self, ctx: &Context, name: &str) -> bool {
        ctx.has_str(name)
    }

    /// Registers a named accessor (mirrors `ctx.accessor(name, { get, set })`).
    ///
    /// The registration is an effect of the calling context's fiber and is
    /// removed when that fiber unloads.
    pub fn accessor(
        &self,
        ctx: &Context,
        name: &str,
        get: MixinGet,
        set: Option<MixinSet>,
    ) -> Result<Rc<EffectHandle>, String> {
        ctx.accessor(name, get, set)
    }
}
