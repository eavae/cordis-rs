//! Cordis plugin loader (Rust port).
//!
//! Replaces the TS `@cordisjs/plugin-loader` package: entry tree management,
//! group/include semantics, and `!expr` config evaluation via minijinja.

pub mod entry;
pub mod evaluator;
pub mod loader;

pub use entry::{Entry, EntryGroup, EntryOptions, EntryTree, IsolateValue, PartialEntryOptions};
pub use evaluator::{
    ConfigEvaluator, EvalEnv, EvalError, MinijinjaEvaluator, evaluate_config, reject_exprs,
};
pub use loader::Loader;
