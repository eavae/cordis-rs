//! Alias for the builtin `@cordisjs/plugin-group` plugin.
//!
//! Mirrors the TS package `@cordisjs/plugin-group`, whose default export is
//! the `Group` class from `@cordisjs/plugin-loader`. The group plugin itself
//! lives in `cordis-loader`; this crate only re-exports the plugin factory.

pub use cordis_loader::group_plugin;
