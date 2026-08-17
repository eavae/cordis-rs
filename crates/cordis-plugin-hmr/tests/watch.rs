//! File watching, debounce, ignored globs and include refresh.

use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use cordis_core::{Context, Effect, EventOptions};
use cordis_loader::{EntryOptions, Loader};
use cordis_plugin_hmr::{FileWatcher, HmrConfig, validate_config};
use cordis_plugin_include::{IncludeConfig, include_plugin};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cordis-hmr-watch-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Waits up to 10s for `check`; the OS file watcher and the debounce timer
/// run on wall-clock time, so the budget must tolerate slow machines.
async fn wait_for(mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("condition not met within 10s");
}

/// Config defaults match the TS `Hmr.Config`.
#[test]
fn config_defaults_and_validation() {
    let config = HmrConfig::default();
    assert_eq!(config.root, vec![".".to_string()]);
    assert_eq!(config.debounce, 100);
    assert_eq!(
        config.ignored,
        vec![
            "**/node_modules".to_string(),
            "**/.*".to_string(),
            "cache".to_string(),
            "data".to_string(),
        ]
    );
    assert!(validate_config(&config).is_ok());

    let mut invalid = config.clone();
    invalid.debounce = 0;
    assert!(validate_config(&invalid).is_err());
}

/// File changes trigger a debounced `hmr/change` event with the path; ignored
/// files are filtered out.
#[tokio::test(flavor = "current_thread")]
async fn change_event_and_ignored() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = temp_dir("event");
            let root = Context::new();
            let loader = Loader::new(&root);
            let changes = Rc::new(std::cell::RefCell::new(Vec::new()));
            drop(
                root.on(
                    "hmr/change",
                    Rc::new({
                        let changes = changes.clone();
                        move |args| {
                            if let Some(path) = args[0].downcast_ref::<String>() {
                                changes.borrow_mut().push(path.clone());
                            }
                            Ok(None)
                        }
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
            );

            let watcher = FileWatcher::start(
                root.clone(),
                loader,
                HmrConfig {
                    base: Some(dir.to_string_lossy().into_owned()),
                    root: vec![".".to_string()],
                    debounce: 30,
                    ignored: vec!["**/node_modules".to_string()],
                },
                dir.clone(),
            );
            fs::create_dir_all(dir.join("node_modules")).unwrap();
            fs::write(dir.join("node_modules/pkg.js"), "x").unwrap();
            fs::write(dir.join("src.js"), "hello").unwrap();

            wait_for(|| !changes.borrow().is_empty()).await;
            watcher.stop();
            assert!(
                changes.borrow().iter().any(|p| p.ends_with("src.js")),
                "src.js change must emit hmr/change: {:?}",
                changes.borrow()
            );
            assert!(
                !changes.borrow().iter().any(|p| p.contains("node_modules")),
                "ignored files must not emit: {:?}",
                changes.borrow()
            );
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

/// A config file owned by the include plugin is refreshed instead of
/// emitting `hmr/change`.
#[tokio::test(flavor = "current_thread")]
async fn include_config_refresh() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = temp_dir("include");
            let config_path = dir.join("base.yml");
            fs::write(
                &config_path,
                "- id: '1'\n  name: greeter\n  config:\n    value: one\n",
            )
            .unwrap();
            let root = Context::new();
            let loader = Loader::new(&root);
            loader
                .builtins
                .borrow_mut()
                .insert("@cordisjs/plugin-include".to_string(), include_plugin());
            loader.mock(
                "greeter",
                Rc::new(|ctx: &Context, config: &Rc<dyn std::any::Any>| {
                    let value = config
                        .downcast_ref::<serde_yaml_ng::Value>()
                        .and_then(|value| value.get("value"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("default")
                        .to_string();
                    drop(ctx.provide_str("greeting", Rc::new(value)).unwrap());
                    Effect::None
                }),
            );
            let changes = Rc::new(std::cell::RefCell::new(0u32));
            drop(
                root.on(
                    "hmr/change",
                    Rc::new({
                        let changes = changes.clone();
                        move |_args| {
                            *changes.borrow_mut() += 1;
                            Ok(None)
                        }
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
            );
            let tree = loader.tree_handle();
            tree.create(
                EntryOptions {
                    id: String::new(),
                    name: "@cordisjs/plugin-include".to_string(),
                    config: Some(
                        serde_yaml_ng::to_value(IncludeConfig {
                            path: config_path.to_string_lossy().into_owned(),
                            initial: None,
                            patches: None,
                            enable_logs: None,
                        })
                        .unwrap(),
                    ),
                    group: None,
                    disabled: None,
                    inject: None,
                    isolate: None,
                    intercept: None,
                    extra: Default::default(),
                },
                None,
                0,
            );
            tree.await_tree().await;
            assert_eq!(
                root.get_str("greeting")
                    .and_then(|v| v.downcast::<String>().ok())
                    .map(|s| s.to_string())
                    .as_deref(),
                Some("one")
            );

            // Change the include config file → refresh, no hmr/change.
            let watcher = FileWatcher::start(
                root.clone(),
                loader.clone(),
                HmrConfig {
                    base: Some(dir.to_string_lossy().into_owned()),
                    root: vec![".".to_string()],
                    debounce: 20,
                    ignored: vec![],
                },
                dir.clone(),
            );
            fs::write(
                &config_path,
                "- id: '1'\n  name: greeter\n  config:\n    value: two\n",
            )
            .unwrap();
            wait_for(|| {
                root.get_str("greeting")
                    .and_then(|v| v.downcast::<String>().ok())
                    .map(|s| s.to_string())
                    .as_deref()
                    == Some("two")
            })
            .await;
            watcher.stop();
            assert_eq!(
                root.get_str("greeting")
                    .and_then(|v| v.downcast::<String>().ok())
                    .map(|s| s.to_string())
                    .as_deref(),
                Some("two"),
                "include config change must refresh the tree"
            );
            assert_eq!(
                *changes.borrow(),
                0,
                "config file changes must not emit hmr/change"
            );
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}
