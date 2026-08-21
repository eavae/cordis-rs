//! EntryTree。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{Context, Effect};
use cordis_loader::{EntryOptions, Loader};

fn opts(id: &str, name: &str) -> EntryOptions {
    EntryOptions {
        id: id.to_string(),
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

#[tokio::test(flavor = "current_thread")]
async fn create_remove_and_write() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let writes = Arc::new(AtomicU32::new(0));
            loader.tree_handle().write_callback.store(Arc::new(Some({
                let writes = writes.clone();
                Arc::new(move || writes.store(writes.load(Ordering::SeqCst) + 1, Ordering::SeqCst))
            })));
            loader.mock("foo", Arc::new(|_ctx, _config| Effect::None));

            let tree = loader.tree_handle();
            let entry = tree.create(opts("", "foo"), None, 0);
            assert_eq!(entry.id().len(), 8, "ensure_id generates 8 hex chars");
            tree.await_tree().await;
            assert_eq!(writes.load(Ordering::SeqCst), 1);
            assert_eq!(tree.entries().len(), 1);

            tree.remove(&entry.id());
            tree.await_tree().await;
            assert_eq!(writes.load(Ordering::SeqCst), 2);
            assert_eq!(tree.entries().len(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn resolve_path_and_errors() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", Arc::new(|_ctx, _config| Effect::None));
            let tree = loader.tree_handle();
            let entry = tree.create(opts("1", "foo"), None, 0);
            tree.await_tree().await;

            let resolved = tree.resolve_path("1").unwrap();
            assert!(Arc::ptr_eq(&resolved, &entry));
            let error = tree.resolve_path("missing").unwrap_err();
            assert_eq!(error, "cannot resolve entry missing");
            let error = tree.resolve_group(Some("1")).unwrap_err();
            assert_eq!(error, "entry 1 is not a group");
            assert!(tree.resolve_group(None).is_ok());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ensure_id_is_unique() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", Arc::new(|_ctx, _config| Effect::None));
            let tree = loader.tree_handle();
            let mut seen = Vec::new();
            for _ in 0..20 {
                let entry = tree.create(opts("", "foo"), None, 0);
                assert!(entry.id().chars().all(|c| c.is_ascii_hexdigit()));
                assert!(!seen.contains(&entry.id()));
                seen.push(entry.id());
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn get_tasks_and_await() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock(
                "blocked",
                Arc::new(|_ctx, _config| {
                    // An async apply that pends until advanced.
                    Effect::None
                }),
            );
            loader.mock("foo", Arc::new(|_ctx, _config| Effect::None));
            let tree = loader.tree_handle();
            let entry = tree.create(opts("1", "foo"), None, 0);
            assert!(tree.get_tasks() > 0, "pending init must be counted");
            tree.await_tree().await;
            assert_eq!(tree.get_tasks(), 0);
            assert!(entry.fiber.lock().unwrap().is_some());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn builtins_import() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            {
                let _guard = loader.tree.write_lock.lock().unwrap();
                let mut builtins = (*loader.builtins.load_full()).clone();
                builtins.insert(
                    "demo".to_string(),
                    cordis_core::Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: Arc::new(|_ctx, _config| Effect::None),
                    },
                );
                loader.builtins.store(Arc::new(builtins));
            }
            let tree = loader.tree_handle();
            assert!(tree.import("cordis:demo").is_ok());
            assert!(tree.import("cordis:missing").is_err());
            assert!(tree.import("no-such-plugin").is_err());
        })
        .await;
}
