//! Cordis core runtime (Rust port).
//!
//! Implements the redesigned API decided in `docs/core-difficulties-solutions.md`:
//! typed + dynamic service access, explicit context passing, and a
//! single-threaded tokio runtime.

pub mod context;
pub mod error;
pub mod events;
pub mod fiber;
pub mod logger;
pub mod reflect;
pub mod registry;
pub mod service;

pub use context::{Context, Label, MixinAccessor, MixinGet, MixinSet, ServiceShadow};
pub use cordis_macros::{inject, service};
pub use error::{ConfigValidator, ValidationError, ValidationIssue};
pub use events::{
    AnyNext, EventCallback, EventFilter, EventOptions, EventsService, ListenerFilter,
    ParallelError, WaterfallNext, event_listener,
};
pub use fiber::{CordisError, EffectHandle, EffectMeta, Fiber, FiberError, FiberState, disposer};
pub use logger::{
    C16, C256, LogFormatter, LogValue, Logger, LoggerExporter, LoggerIntercept, LoggerLevel,
    LoggerService, LoggerType, Message, SimpleExporter, format_message, hyphenate,
};
pub use reflect::ReflectService;
pub use registry::{Plugin, RegistryService};
pub use service::{
    ApplyFn, AsyncDisposerStream, BoxFuture, Config, Disposer, Effect, EffectItem, Service,
    async_disposer, sync_disposer,
};
