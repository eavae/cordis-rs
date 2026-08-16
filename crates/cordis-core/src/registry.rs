//! Registry service (shell).
//!
//! Story card B4 implements plugin registration and runtime management. This
//! card only provides the service identity so that `ctx.registry` resolves.

use crate::service::Service;

/// Registry service, available on every context as `ctx.registry`.
#[derive(Debug)]
pub struct RegistryService;

impl Service for RegistryService {
    const NAME: &'static str = "registry";
}
