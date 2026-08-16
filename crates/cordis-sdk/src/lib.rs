//! Cordis plugin SDK.
//!
//! The only crate a `.so` plugin needs to depend on. Exposes the plugin
//! contract (`Plugin`, `Context`, spawn/effect/logger host entry points) and
//! re-exports core types once the core API is frozen (feature
//! `re-export-core`).
