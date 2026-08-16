//! Logger service (shell).
//!
//! Story card B7 implements levels, formatting, colors and exporters. This
//! card only provides the service identity so that `ctx.logger` resolves.

use crate::service::Service;

/// Logger service, available on every context as `ctx.logger`.
#[derive(Debug)]
pub struct LoggerService;

impl Service for LoggerService {
    const NAME: &'static str = "logger";
}
