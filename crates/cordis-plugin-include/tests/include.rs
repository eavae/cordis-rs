//! Story card D1: include 插件.

use std::fs;
use std::rc::Rc;

use cordis_core::{Context, Effect};
use cordis_loader::{EntryOptions, Loader};
use cordis_plugin_include::{IncludeConfig, Override, PatchOptions, include_plugin};

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
}

fn greeting(root: &Context) -> Option<String> {
    root.get_str("greeting")
        .and_then(|value| value.downcast::<String>().ok())
        .map(|value| value.to_string())
}

#[tokio::test(flavor = "current_thread")]
async fn loads_without_patches() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-d1a-{}", std::process::id()));
            let path = write_fixture(
                &dir,
                "base.yml",
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
            );
            let root = Context::new();
            let loader = Loader::new(&root);
            setup_loader(&loader);
            let tree = loader.tree_handle();
            let entry = tree.create(
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
            assert_eq!(greeting(&root).as_deref(), Some("hello"));
            assert!(!entry.id().is_empty());
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn patch_disables_entry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-d1b-{}", std::process::id()));
            let path = write_fixture(
                &dir,
                "base.yml",
                "- id: '1'\n  name: greeter\n  config:\n    value: hello\n",
            );
            let root = Context::new();
            let loader = Loader::new(&root);
            setup_loader(&loader);
            let tree = loader.tree_handle();
            tree.create(
                include_opts(IncludeConfig {
                    path,
                    initial: None,
                    patches: Some(vec![PatchOptions {
                        id: Some("1".to_string()),
                        disabled: Override::Set(true),
                        ..Default::default()
                    }]),
                    enable_logs: None,
                }),
                None,
                0,
            );
            tree.await_tree().await;
            assert!(
                greeting(&root).is_none(),
                "patched-disabled entry must not apply"
            );
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn missing_file_with_initial_creates_it() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let dir = std::env::temp_dir().join(format!("cordis-d1c-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join("missing.yml").to_string_lossy().to_string();
            let root = Context::new();
            let loader = Loader::new(&root);
            setup_loader(&loader);
            let tree = loader.tree_handle();
            tree.create(
                include_opts(IncludeConfig {
                    path,
                    initial: Some(
                        serde_yaml_ng::from_str::<Vec<EntryOptions>>(
                            "- id: '1'\n  name: greeter\n  config:\n    value: created\n",
                        )
                        .unwrap(),
                    ),
                    patches: None,
                    enable_logs: None,
                }),
                None,
                0,
            );
            tree.await_tree().await;
            assert_eq!(greeting(&root).as_deref(), Some("created"));
            assert!(
                dir.join("missing.yml").exists(),
                "initial file must be created"
            );
            fs::remove_dir_all(&dir).unwrap();
        })
        .await;
}
