//! Cordis core runtime (Rust port).
//!
//! Implements the redesigned API decided in `docs/core-difficulties-solutions.md`:
//! typed + dynamic service access, explicit context passing, and a
//! single-threaded tokio runtime.

pub mod context;
pub mod events;
pub mod fiber;
pub mod logger;
pub mod reflect;
pub mod registry;
pub mod service;

pub use context::Context;
pub use events::EventsService;
pub use fiber::{Fiber, FiberState};
pub use logger::LoggerService;
pub use reflect::ReflectService;
pub use registry::RegistryService;
pub use service::{Config, Disposer, Service};
