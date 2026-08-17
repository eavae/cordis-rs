//! Plugin metadata: the JSON payload returned by a `.so` plugin's
//! `plugin_meta` export.

use serde::Deserialize;

/// The metadata a plugin advertises through `plugin_meta()`.
#[derive(Clone, Debug, Deserialize)]
pub struct PluginMeta {
    /// Stable plugin id/name used as the loader registry key.
    pub name: String,
    /// Plugin version.
    #[serde(default)]
    pub version: Option<String>,
    /// Declared inject dependencies.
    #[serde(default)]
    pub inject: Vec<String>,
    /// Declared provided services.
    #[serde(default)]
    pub provide: Vec<String>,
    /// Declared dependencies: host crates/services this plugin links
    /// against.
    #[serde(default)]
    pub deps: Vec<String>,
}
