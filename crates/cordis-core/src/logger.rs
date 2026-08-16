//! Logger service.
//!
//! Story card B2 provides an error sink used by the fiber lifecycle; story
//! card B7 implements levels, formatting, colors and exporters.

use std::cell::RefCell;
use std::fmt;

use crate::service::Service;

/// Logger service, available on every context as `ctx.logger`.
#[derive(Default)]
pub struct LoggerService {
    errors: RefCell<Vec<String>>,
}

impl LoggerService {
    /// Records an error (B7 replaces this with exporter dispatch).
    pub fn error(&self, message: impl fmt::Display) {
        self.errors.borrow_mut().push(message.to_string());
    }

    /// Number of recorded errors (used by B2 tests to assert error counts).
    pub fn error_count(&self) -> usize {
        self.errors.borrow().len()
    }
}

impl Service for LoggerService {
    const NAME: &'static str = "logger";
}
