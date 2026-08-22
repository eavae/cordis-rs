//! Cordis core runtime (Rust port).
//!
//! Implements the redesigned API decided in `docs/core-difficulties-solutions.md`:
//! typed + dynamic service access, explicit context passing, and a
//! single-threaded tokio runtime.

pub mod context;
pub mod deadlock;
pub mod error;
pub mod events;
pub mod fiber;
pub mod logger;
pub mod reflect;
pub mod registry;
pub mod service;

pub use context::{
    Context, Label, MixinAccessor, MixinGet, MixinSet, ServiceShadow, ShadowContext,
};
pub use cordis_macros::{inject, service};
pub use error::{ConfigValidator, ValidationError, ValidationIssue};
pub use events::{
    EventCallback, EventFilter, EventOptions, EventsService, ListenerFilter, ParallelError,
    WaterfallNext, event_callback, event_listener, event_listener_async,
};
pub use fiber::{CordisError, EffectHandle, EffectMeta, Fiber, FiberError, FiberState, disposer};
pub use logger::{
    COLOR_16, COLOR_256, LogFormatter, LogValue, Logger, LoggerExporter, LoggerIntercept,
    LoggerLevel, LoggerService, LoggerType, Message, SimpleExporter, UnknownLoggerLevel,
    format_message, hyphenate,
};
pub use reflect::ReflectService;
pub use registry::{
    Plugin, PluginSpec, RegistryService, plugin_async, plugin_sync, typed_validator,
};
pub use service::{
    ApplyFn, AsyncDisposerStream, BoxError, BoxFuture, Config, Disposer, Effect, EffectItem,
    PluginOutput, Service, async_disposer, sync_disposer,
};
