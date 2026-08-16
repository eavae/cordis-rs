//! Event dispatch service (shell).
//!
//! Story card B5 implements the five dispatch modes (`emit`, `parallel`,
//! `serial`, `bail`, `waterfall`) and listener registration. This card only
//! provides the service identity so that `ctx.events` resolves.

use crate::service::Service;

/// Event dispatch service, available on every context as `ctx.events`.
#[derive(Debug)]
pub struct EventsService;

impl Service for EventsService {
    const NAME: &'static str = "events";
}
