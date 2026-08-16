//! Reflect service (shell).
//!
//! Story card B8/B9/B10 build the property table, accessors and mixins on top
//! of this service. This card only provides the service identity so that
//! `ctx.reflect` resolves.

use crate::service::Service;

/// Reflect service, available on every context as `ctx.reflect`.
#[derive(Debug)]
pub struct ReflectService;

impl Service for ReflectService {
    const NAME: &'static str = "reflect";
}
