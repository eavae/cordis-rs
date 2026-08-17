//! Story card D1 补全：include patch 缺口回归（8 条）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::rc::Rc;
use std::time::Duration;

use cordis_core::{Context, Effect, LoggerLevel, SimpleExporter};
use cordis_loader::{EntryOptions, IsolateValue, Loader};
use cordis_plugin_include::{IncludeConfig, Override, PatchOptions, include_plugin};
use serde_yaml_ng::Value;

fn write_fixture(dir: &std::path::Path, name: &str, content: &str) -> String {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, content).unwrap();
    path.to_string_lossy().to_string()
}

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
        extra: Default::default(),
    }
}

fn setup_loader(loader: &Loader) {
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
    loader.mock("noop", Rc::new(|_ctx: &Context, _config| Effect::None));
}

fn greeting(root: &Context) -> Option<String> {
    root.get_str("greeting")
        .and_then(|value| value.downcast::<String>().ok())
        .map(|value| value.to_string())
}

fn config_value(value: &str) -> Value {
    serde_yaml_ng::to_value(serde_json::json!({ "value": value })).unwrap()
}

fn entry_by_id(loader: &Loader, id: &str) -> Rc<cordis_loader::Entry> {
    loader
        .tree_handle()
        .entries()
        .into_iter()
        .find(|entry| entry.options.borrow().id == id)
        .unwrap_or_else(|| panic!("entry {id} not found"))
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met");
}

async fn setup_include(
    dir: &std::path::Path,
    fixture: &str,
    patches: Vec<PatchOptions>,
) -> (Context, Rc<Loader>, String) {
    let path = write_fixture(dir, "base.yml", fixture);
    let root = Context::new();
    let loader = Loader::new(&root);
    setup_loader(&loader);
    let tree = loader.tree_handle();
    tree.create(
        include_opts(IncludeConfig {
            path: path.clone(),
            initial: None,
            patches: Some(patches),
            enable_logs: None,
        }),
        None,
        0,
    );
    tree.await_tree().await;
    (root, loader, path)
}

async fn mount_and_capture(
    dir: &std::path::Path,
    fixture: &str,
    patches: Vec<PatchOptions>,
) -> (Context, Rc<Loader>, Rc<RefCell<Vec<String>>>, String) {
    let path = write_fixture(dir, "base.yml", fixture);
    let root = Context::new();
    let captured = Rc::new(RefCell::new(Vec::new()));
    drop(
        root.logger()
            .exporter(Rc::new(SimpleExporter {
                colors: 0,
                max_length: 10240,
                levels: Some(Rc::new(HashMap::from([(
                    "default".to_string(),
                    LoggerLevel::Debug,
                )]))),
                formatters: None,
                handler: {
                    let captured = captured.clone();
                    Rc::new(move |message| captured.borrow_mut().push(message.args[0].inspect()))
                },
            }))
            .unwrap(),
    );
    let loader = Loader::new(&root);
    setup_loader(&loader);
    let tree = loader.tree_handle();
    tree.create(
        include_opts(IncludeConfig {
            path: path.clone(),
            initial: None,
            patches: Some(patches),
            enable_logs: None,
        }),
        None,
        0,
    );
    tree.await_tree().await;
    (root, loader, captured, path)
}

#[tokio::test(flavor = "current_thread")]
async fn patch_overrides_group_inject_intercept_isolate() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1a-{}", std::process::id()));
            let mut patch = PatchOptions {
                id: Some("1".to_string()),
                ..Default::default()
            };
            patch.group = Override::Set(true);
            patch.inject = Override::Set(vec!["foo".to_string()]);
            patch.intercept =
                Override::Set(serde_yaml_ng::to_value(serde_json::json!({ "a": 1 })).unwrap());
            patch.isolate = Override::Set(HashMap::from([(
                "svc".to_string(),
                IsolateValue::Label("shared".to_string()),
            )]));
            let (root, loader, _path) = setup_include(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;

            let options = entry_by_id(&loader, "1").options.borrow().clone();
            assert_eq!(options.group, Some(true));
            assert_eq!(options.inject.as_deref(), Some(&["foo".to_string()][..]));
            assert_eq!(
                options.intercept,
                Some(serde_yaml_ng::to_value(serde_json::json!({ "a": 1 })).unwrap())
            );
            assert_eq!(
                options.isolate.as_ref().and_then(|map| map.get("svc")),
                Some(&IsolateValue::Label("shared".to_string()))
            );
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_writes_arbitrary_extra_key_and_round_trips() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1b-{}", std::process::id()));
            let mut patch = PatchOptions {
                id: Some("1".to_string()),
                ..Default::default()
            };
            patch
                .extra
                .insert("custom".to_string(), Value::String("x".to_string()));
            let (root, loader, path) = setup_include(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;

            let options = entry_by_id(&loader, "1").options.borrow().clone();
            assert_eq!(
                options.extra.get("custom"),
                Some(&Value::String("x".to_string()))
            );

            // The unknown key must survive a write-back cycle.
            loader.tree_handle().write();
            wait_until(|| {
                fs::read_to_string(&path)
                    .map(|content| content.contains("custom: x"))
                    .unwrap_or(false)
            })
            .await;
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains("custom: x"), "extra key not written back");
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_targets_two_level_nested_entry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1c-{}", std::process::id()));
            let fixture = r#"
- id: outer
  name: "@cordisjs/plugin-group"
  group: true
  config:
    - id: inner
      name: "@cordisjs/plugin-group"
      group: true
      config:
        - id: leaf
          name: greeter
          config:
            value: before
"#;
            let patch = PatchOptions {
                id: Some("leaf".to_string()),
                config: Override::Set(config_value("after")),
                ..Default::default()
            };
            let (root, _loader, _path) = setup_include(&dir, fixture, vec![patch]).await;
            assert_eq!(greeting(&root).as_deref(), Some("after"));
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_inserts_into_nested_subgroup() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1d-{}", std::process::id()));
            let fixture = r#"
- id: outer
  name: "@cordisjs/plugin-group"
  group: true
  config:
    - id: inner
      name: "@cordisjs/plugin-group"
      group: true
      config:
        - id: leaf
          name: greeter
          config:
            value: before
"#;
            let patch = PatchOptions {
                id: Some("inner".to_string()),
                insert: Some(vec![EntryOptions {
                    id: "added".to_string(),
                    name: "noop".to_string(),
                    config: Some(config_value("added")),
                    group: None,
                    disabled: None,
                    inject: None,
                    isolate: None,
                    intercept: None,
                    extra: Default::default(),
                }]),
                ..Default::default()
            };
            let (root, loader, _path) = setup_include(&dir, fixture, vec![patch]).await;

            // The inserted entry must live inside the nested subgroup.
            assert!(
                loader
                    .tree_handle()
                    .entries()
                    .iter()
                    .any(|entry| entry.options.borrow().id == "added"),
                "inserted entry missing"
            );
            assert_eq!(greeting(&root).as_deref(), Some("before"));
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_null_clears_disabled_and_config() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1e-{}", std::process::id()));
            let patch = PatchOptions {
                id: Some("1".to_string()),
                disabled: Override::Clear,
                config: Override::Clear,
                ..Default::default()
            };
            let (root, loader, _path) = setup_include(
                &dir,
                "- id: '1'\n  name: greeter\n  disabled: true\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;

            let options = entry_by_id(&loader, "1").options.borrow().clone();
            assert_eq!(options.disabled, None);
            assert_eq!(options.config, None);
            // Cleared config falls back to the plugin default.
            assert_eq!(greeting(&root).as_deref(), Some("default"));
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_after_root_insert_matches_js_semantics() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1f-{}", std::process::id()));
            let insert = PatchOptions {
                insert: Some(vec![EntryOptions {
                    id: "new1".to_string(),
                    name: "noop".to_string(),
                    config: Some(config_value("original")),
                    group: None,
                    disabled: None,
                    inject: None,
                    isolate: None,
                    intercept: None,
                    extra: Default::default(),
                }]),
                ..Default::default()
            };
            // JS semantics: entries inserted by an earlier patch are not
            // addressable by later patches ("not found").
            let not_found = PatchOptions {
                id: Some("new1".to_string()),
                config: Override::Set(config_value("patched")),
                ..Default::default()
            };
            // Existing entries must stay addressable despite the index shift.
            let existing = PatchOptions {
                id: Some("1".to_string()),
                config: Override::Set(config_value("updated")),
                ..Default::default()
            };
            let (root, loader, _path) = setup_include(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![insert, not_found, existing],
            )
            .await;

            assert_eq!(greeting(&root).as_deref(), Some("updated"));
            let inserted = entry_by_id(&loader, "new1").options.borrow().clone();
            assert_eq!(
                inserted.config,
                Some(config_value("original")),
                "entry inserted by an earlier patch must not be re-patched"
            );
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_name_mismatch_warns_and_skips() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1g-{}", std::process::id()));
            let patch = PatchOptions {
                id: Some("1".to_string()),
                name: Some("wrong-name".to_string()),
                disabled: Override::Set(true),
                ..Default::default()
            };
            let (root, _loader, captured, _path) = mount_and_capture(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;
            assert!(
                captured
                    .borrow()
                    .iter()
                    .any(|line| line.contains("name mismatch")),
                "{:?}",
                captured.borrow()
            );
            assert_eq!(greeting(&root).as_deref(), Some("hello"), "patch skipped");
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_matching_name_applies() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1h-{}", std::process::id()));
            let patch = PatchOptions {
                id: Some("1".to_string()),
                name: Some("greeter".to_string()),
                disabled: Override::Set(true),
                ..Default::default()
            };
            let (root, _loader, captured, _path) = mount_and_capture(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;
            assert!(
                !captured
                    .borrow()
                    .iter()
                    .any(|line| line.contains("name mismatch")),
                "{:?}",
                captured.borrow()
            );
            assert_eq!(greeting(&root), None, "matching-name patch must apply");
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_nonexistent_id_warns() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1i-{}", std::process::id()));
            let patch = PatchOptions {
                id: Some("nope".to_string()),
                disabled: Override::Set(true),
                ..Default::default()
            };
            let (root, _loader, captured, _path) = mount_and_capture(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;
            assert!(
                captured
                    .borrow()
                    .iter()
                    .any(|line| line.contains("entry nope not found")),
                "{:?}",
                captured.borrow()
            );
            assert_eq!(greeting(&root).as_deref(), Some("hello"));
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_insert_into_non_group_warns() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-g1j-{}", std::process::id()));
            let patch = PatchOptions {
                id: Some("1".to_string()),
                insert: Some(vec![EntryOptions {
                    id: "extra".to_string(),
                    name: "noop".to_string(),
                    config: None,
                    group: None,
                    disabled: None,
                    inject: None,
                    isolate: None,
                    intercept: None,
                    extra: Default::default(),
                }]),
                ..Default::default()
            };
            let (root, loader, captured, _path) = mount_and_capture(
                &dir,
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
                vec![patch],
            )
            .await;
            assert!(
                captured
                    .borrow()
                    .iter()
                    .any(|line| line.contains("is not a group")),
                "{:?}",
                captured.borrow()
            );
            assert!(
                !loader
                    .tree_handle()
                    .entries()
                    .iter()
                    .any(|entry| entry.options.borrow().id == "extra"),
                "inserted entry must not be mounted"
            );
            assert_eq!(greeting(&root).as_deref(), Some("hello"));
            drop(root);
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}
