//! Reload execution (entry re-apply) and rollback.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{Context, Effect, Plugin};
use cordis_loader::{EntryOptions, Loader};

fn opts(name: &str) -> EntryOptions {
    EntryOptions {
        id: String::new(),
        name: name.to_string(),
        config: None,
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

fn plugin(applied: Arc<AtomicU32>) -> Plugin {
    Plugin {
        is_group: false,
        name: None,
        inject: Vec::new(),
        apply: Arc::new(move |_ctx: &Context, _config| {
            applied.store(applied.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            Effect::None
        }),
    }
}

/// Replacing the plugin under an entry's name and calling `reload()`
/// re-applies the entry (fiber bound to the new apply, config preserved).
#[tokio::test(flavor = "current_thread")]
async fn entry_reload_reapplies_with_new_plugin() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Arc::new(AtomicU32::new(0));
            loader.tree.plugins.rcu(|plugins| {
                let mut next = (**plugins).clone();
                next.insert("p".to_string(), plugin(applied.clone()));
                Arc::new(next)
            });
            let tree = loader.tree_handle();
            let entry = tree.create(opts("p"), None, 0);
            tree.await_tree().await;
            assert_eq!(applied.load(Ordering::SeqCst), 1);
            let config_before = entry.options.lock().unwrap().config.clone();

            // Swap in a new apply and reload the entry.
            loader.tree.plugins.rcu(|plugins| {
                let mut next = (**plugins).clone();
                next.insert("p".to_string(), plugin(applied.clone()));
                Arc::new(next)
            });
            entry.reload().await.expect("reload");
            assert_eq!(
                applied.load(Ordering::SeqCst),
                2,
                "reload must re-apply the entry"
            );
            assert_eq!(
                entry.options.lock().unwrap().config,
                config_before,
                "entry options (config) must be preserved"
            );
        })
        .await;
}

/// A failing reload rolls back to the previous plugin.
#[tokio::test(flavor = "current_thread")]
async fn reload_rolls_back_on_failure() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Arc::new(AtomicU32::new(0));
            loader.tree.plugins.rcu(|plugins| {
                let mut next = (**plugins).clone();
                next.insert("p".to_string(), plugin(applied.clone()));
                Arc::new(next)
            });
            let tree = loader.tree_handle();
            let entry = tree.create(opts("p"), None, 0);
            tree.await_tree().await;
            assert_eq!(applied.load(Ordering::SeqCst), 1);

            // The "new artifact" fails on apply.
            loader.tree.plugins.rcu(|plugins| {
                let mut next = (**plugins).clone();
                next.insert(
                    "p".to_string(),
                    Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: Arc::new(|_ctx: &Context, _config| {
                            Effect::Error("boom".to_string().into())
                        }),
                    },
                );
                Arc::new(next)
            });
            assert!(entry.reload().await.is_err(), "reload must fail");

            // Roll back to the previous plugin and re-apply.
            loader.tree.plugins.rcu(|plugins| {
                let mut next = (**plugins).clone();
                next.insert("p".to_string(), plugin(applied.clone()));
                Arc::new(next)
            });
            entry.reload().await.expect("rollback reload");
            assert_eq!(
                applied.load(Ordering::SeqCst),
                2,
                "rollback must restore the previous plugin"
            );
        })
        .await;
}
