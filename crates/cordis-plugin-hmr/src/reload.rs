//! Reload execution with rollback.
//!
//! Unlike the TS "clear module caches" model, the Rust port swaps plugin
//! handles atomically: the new artifact is registered first and each affected
//! entry re-applies; on failure the old artifact is restored.

use std::sync::Arc;

use cordis_core::Context;
use cordis_loader::{Loader, SoPlugin};

/// One reload request: swap the plugin registered under `name`.
pub struct ReloadRequest {
    /// The loader registry key of the plugin.
    pub name: String,
    /// The new artifact (already loaded and validated).
    pub next: SoPlugin,
    /// The artifact to restore on rollback.
    pub previous: Option<SoPlugin>,
}

/// The reload result.
#[derive(Debug, Default)]
pub struct ReloadReport {
    /// Plugin names that reloaded successfully.
    pub reloaded: Vec<String>,
    /// Plugin names that failed and were rolled back.
    pub rolled_back: Vec<String>,
}

/// Executes reload requests. Returns the report; on per-plugin failure the
/// old artifact is restored and the loader keeps serving the previous
/// version (state preserved).
pub async fn execute_reloads(
    ctx: &Context,
    loader: &Loader,
    requests: Vec<ReloadRequest>,
) -> ReloadReport {
    let mut report = ReloadReport::default();
    for request in requests {
        let ReloadRequest {
            name,
            next,
            previous,
        } = request;
        let entries: Vec<Arc<cordis_loader::Entry>> = loader
            .tree_handle()
            .entries()
            .into_iter()
            .filter(|entry| entry.options.lock().unwrap().name == name)
            .collect();
        if entries.is_empty() {
            // Not mounted: register the new artifact so future reads use it.
            let _ = loader.register_so_plugin(&next);
            report.reloaded.push(name);
            continue;
        }

        // 1. Register the new artifact (the loader now imports the new
        //    apply); keep the old artifact alive for rollback.
        if let Err(error) = loader.register_so_plugin(&next) {
            ctx.logger()
                .warn(format!("hmr: reload {name} failed to register: {error}"));
            report.rolled_back.push(name);
            continue;
        }

        // 2. Re-apply every entry bound to this plugin.
        let mut failed = false;
        for entry in &entries {
            if let Err(error) = entry.reload().await {
                ctx.logger()
                    .warn(format!("hmr: reload {name} failed: {error}"));
                failed = true;
            }
        }

        if failed {
            // 3. Roll back: restore the previous artifact and re-apply.
            if let Some(previous) = previous {
                let _ = loader.register_so_plugin(&previous);
            }
            for entry in &entries {
                let _ = entry.reload().await;
            }
            ctx.logger()
                .warn(format!("hmr: reload {name} failed; rolled back"));
            report.rolled_back.push(name);
        } else {
            report.reloaded.push(name);
        }
    }
    report
}
