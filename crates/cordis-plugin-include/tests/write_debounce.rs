//! Include write debounce and the `loader/config-update` event.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{Context, Effect, EventOptions, event_callback};
use cordis_loader::{EntryOptions, Loader};
use cordis_plugin_include::{IncludeConfig, include_plugin};

fn include_opts(config: IncludeConfig) -> EntryOptions {
    EntryOptions {
        id: String::new(),
        name: "@cordisjs/plugin-include".to_string(),
        config: Some(serde_yaml_ng::to_value(config).unwrap()),
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

fn setup_loader(loader: &Loader) {
    loader.tree.builtins.rcu(|builtins| {
        let mut next = (**builtins).clone();
        next.insert("@cordisjs/plugin-include".to_string(), include_plugin());
        Arc::new(next)
    });
    loader.mock("greeter", Arc::new(|_ctx: &Context, _config| Effect::None));
}

fn fixture_yaml(dir: &std::path::Path, value: i64) -> String {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("base.yml");
    fs::write(
        &path,
        format!("- id: '1'\n  name: greeter\n  config:\n    value: {value}\n"),
    )
    .unwrap();
    path.to_string_lossy().to_string()
}

fn config_value(a: i64) -> serde_yaml_ng::Value {
    let mut map = serde_yaml_ng::Mapping::new();
    map.insert(
        serde_yaml_ng::Value::String("value".to_string()),
        serde_yaml_ng::Value::Number(a.into()),
    );
    serde_yaml_ng::Value::Mapping(map)
}

/// Waits until `check` is true (single-threaded runtime: yields drive the
/// debounced write task).
async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not met within 500 yields");
}

/// Multiple `write()` calls in the same turn coalesce into one disk write;
/// `loader/config-update` fires on every call.
#[tokio::test(flavor = "current_thread")]
async fn same_turn_writes_coalesce_but_events_fire() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir()
                .join(format!("cordis-include-debounce-a-{}", std::process::id()));
            let path = fixture_yaml(&dir, 1);
            let root = Context::new();
            let updates = Arc::new(AtomicU32::new(0));
            drop(
                root.on(
                    "loader/config-update",
                    event_callback({
                        let updates = updates.clone();
                        move |_args| {
                            updates.store(updates.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            Ok(None)
                        }
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
            );
            let loader = Loader::new(&root);
            setup_loader(&loader);
            let tree = loader.tree_handle();
            tree.create(
                include_opts(IncludeConfig {
                    path,
                    initial: None,
                    patches: None,
                    enable_logs: None,
                }),
                None,
                0,
            );
            tree.await_tree().await;
            updates.store(0, Ordering::SeqCst);

            // Change the entry config, then write twice in the same turn.
            let greeter = tree
                .entries()
                .into_iter()
                .find(|entry| entry.options.lock().unwrap().name == "greeter")
                .expect("greeter entry");
            greeter.options.lock().unwrap().config = Some(config_value(3));
            tree.write();
            tree.write();

            let file = fs::read_to_string(dir.join("base.yml")).unwrap();
            assert!(
                !file.contains("value: 3"),
                "write must be debounced (not yet flushed)"
            );
            assert_eq!(
                updates.load(Ordering::SeqCst),
                2,
                "loader/config-update fires on every write() call"
            );

            tokio::task::yield_now().await;
            wait_until(|| {
                fs::read_to_string(dir.join("base.yml"))
                    .is_ok_and(|content| content.contains("value: 3"))
            })
            .await;
            let content = fs::read_to_string(dir.join("base.yml")).unwrap();
            assert!(
                content.contains("value: 3"),
                "final config must be written once"
            );
            assert_eq!(content.matches("name: greeter").count(), 1);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

/// Writes across turns are not coalesced — each turn flushes.
#[tokio::test(flavor = "current_thread")]
async fn cross_turn_writes_flush_separately() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir()
                .join(format!("cordis-include-debounce-b-{}", std::process::id()));
            let path = fixture_yaml(&dir, 1);
            let root = Context::new();
            let loader = Loader::new(&root);
            setup_loader(&loader);
            let tree = loader.tree_handle();
            tree.create(
                include_opts(IncludeConfig {
                    path,
                    initial: None,
                    patches: None,
                    enable_logs: None,
                }),
                None,
                0,
            );
            tree.await_tree().await;

            let greeter = tree
                .entries()
                .into_iter()
                .find(|entry| entry.options.lock().unwrap().name == "greeter")
                .expect("greeter entry");
            greeter.options.lock().unwrap().config = Some(config_value(2));
            tree.write();
            wait_until(|| {
                fs::read_to_string(dir.join("base.yml"))
                    .is_ok_and(|content| content.contains("value: 2"))
            })
            .await;

            // A later turn triggers its own write.
            greeter.options.lock().unwrap().config = Some(config_value(4));
            tree.write();
            wait_until(|| {
                fs::read_to_string(dir.join("base.yml"))
                    .is_ok_and(|content| content.contains("value: 4"))
            })
            .await;
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

/// A readonly config file is not overwritten (the error path mirrors the TS
/// `cannot overwrite readonly config`).
#[tokio::test(flavor = "current_thread")]
async fn readonly_config_is_not_overwritten() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir()
                .join(format!("cordis-include-debounce-c-{}", std::process::id()));
            let path = fixture_yaml(&dir, 1);
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&path, permissions).unwrap();

            let root = Context::new();
            let loader = Loader::new(&root);
            setup_loader(&loader);
            let tree = loader.tree_handle();
            tree.create(
                include_opts(IncludeConfig {
                    path: path.clone(),
                    initial: None,
                    patches: None,
                    enable_logs: None,
                }),
                None,
                0,
            );
            tree.await_tree().await;

            let greeter = tree
                .entries()
                .into_iter()
                .find(|entry| entry.options.lock().unwrap().name == "greeter")
                .expect("greeter entry");
            greeter.options.lock().unwrap().config = Some(config_value(9));
            tree.write();
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;

            let content = fs::read_to_string(&path).unwrap();
            assert!(
                content.contains("value: 1"),
                "readonly config must not be overwritten"
            );
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}
