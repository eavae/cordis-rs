//! Cordis plugin loader (Rust port).
//!
//! Replaces the TS `@cordisjs/plugin-loader` package: entry tree management,
//! group/include semantics, and `!expr` config evaluation via minijinja.

pub mod config;
pub mod entry;
pub mod evaluator;
pub mod host_runtime;
pub mod loader;
pub mod plugin_meta;
pub mod so;

pub use config::{atomic_write, parse_config, serialize_config, to_sorted_value};
pub use entry::{Entry, EntryGroup, EntryOptions, EntryTree, IsolateValue, PartialEntryOptions};
pub use evaluator::{
    ConfigEvaluator, EvalEnv, EvalError, MinijinjaEvaluator, evaluate_config, reject_exprs,
};
pub use host_runtime::{HostRuntime, host_spawn};
pub use loader::{Loader, LoaderIntercept, group_plugin};
pub use plugin_meta::PluginMeta;
pub use so::{LoadError, SoPlugin, host_vtable, is_plugin_path};
