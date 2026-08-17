//! Story card F3: reload execution (entry re-apply) and rollback.

use std::rc::Rc;

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
        extra: Default::default(),
    }
}

fn plugin(applied: Rc<std::cell::Cell<u32>>) -> Plugin {
    Plugin {
        is_group: false,
        name: None,
        inject: Vec::new(),
        apply: Rc::new(move |_ctx: &Context, _config| {
            applied.set(applied.get() + 1);
            Effect::None
        }),
    }
}

/// F3.1: replacing the plugin under an entry's name and calling `reload()`
/// re-applies the entry (fiber bound to the new apply, config preserved).
#[tokio::test(flavor = "current_thread")]
async fn entry_reload_reapplies_with_new_plugin() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(std::cell::Cell::new(0u32));
            loader
                .tree
                .plugins
                .borrow_mut()
                .insert("p".to_string(), plugin(applied.clone()));
            let tree = loader.tree_handle();
            let entry = tree.create(opts("p"), None, 0);
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            let config_before = entry.options.borrow().config.clone();

            // Swap in a new apply and reload the entry.
            loader
                .tree
                .plugins
                .borrow_mut()
                .insert("p".to_string(), plugin(applied.clone()));
            entry.reload().await.expect("reload");
            assert_eq!(applied.get(), 2, "reload must re-apply the entry");
            assert_eq!(
                entry.options.borrow().config,
                config_before,
                "entry options (config) must be preserved"
            );
        })
        .await;
}

/// F3.2: a failing reload rolls back to the previous plugin.
#[tokio::test(flavor = "current_thread")]
async fn reload_rolls_back_on_failure() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(std::cell::Cell::new(0u32));
            loader
                .tree
                .plugins
                .borrow_mut()
                .insert("p".to_string(), plugin(applied.clone()));
            let tree = loader.tree_handle();
            let entry = tree.create(opts("p"), None, 0);
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);

            // The "new artifact" fails on apply.
            loader.tree.plugins.borrow_mut().insert(
                "p".to_string(),
                Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|_ctx: &Context, _config| {
                        Effect::Error("boom".to_string().into())
                    }),
                },
            );
            assert!(entry.reload().await.is_err(), "reload must fail");

            // Roll back to the previous plugin and re-apply.
            loader
                .tree
                .plugins
                .borrow_mut()
                .insert("p".to_string(), plugin(applied.clone()));
            entry.reload().await.expect("rollback reload");
            assert_eq!(
                applied.get(),
                2,
                "rollback must restore the previous plugin"
            );
        })
        .await;
}
