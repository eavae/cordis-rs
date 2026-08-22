//! `.so` plugin metadata, config validation and the apply bridge.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cordis_core::{Context, Effect, FiberState};
use cordis_loader::{EntryOptions, EvalEnv, Loader, MinijinjaEvaluator, SoPlugin, evaluate_config};

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Serializes tests that share the fixture's process-global state.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn fixture_path() -> PathBuf {
    // `cargo test` does not emit fixture cdylibs; build them on demand.
    cordis_fixture_builder::ensure_fixtures(&["cordis-fixture-meta"]);
    cordis_fixture_builder::artifact_dir().join(if cfg!(target_os = "macos") {
        "libcordis_fixture_meta.dylib"
    } else {
        "libcordis_fixture_meta.so"
    })
}

extern "C" fn log_message(message: *const std::ffi::c_char) {
    // SAFETY: the plugin passes a NUL-terminated string.
    let text = unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .to_string();
    LOGGED.lock().push(text);
}

fn opts(name: &str, config: serde_yaml_ng::Value) -> EntryOptions {
    EntryOptions {
        id: String::new(),
        name: name.to_string(),
        config: Some(config),
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

/// Metadata, bridging, apply and validation run in one sequential test (the
/// fixture's process-global counters/log are shared).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn meta_config_apply_and_validation() {
    let _guard = FIXTURE_LOCK.lock();
    async {
        // Host reads id/name/version/inject/provide from `plugin_meta`.
        let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
        let handle = unsafe { plugin.create(log_message) };
        assert!(!handle.is_null());
        let metadata = plugin.metadata().expect("meta symbol").expect("valid json");
        assert_eq!(metadata.name, "cordis-meta");
        assert_eq!(metadata.version.as_deref(), Some("1.0.0"));
        assert_eq!(metadata.inject, vec!["logger".to_string()]);
        assert_eq!(metadata.provide, vec!["greeting".to_string()]);
        assert!(plugin.validator().is_some(), "validate symbol exported");
        assert!(plugin.apply_fn().is_some(), "apply symbol exported");

        // Bridged into the loader; inject flows into the fiber; valid
        // configs apply through the FFI apply entry.
        let root = Context::new();
        let loader = Loader::new(&root);
        let name = loader.register_so_plugin(&plugin).expect("register");
        assert_eq!(name, "cordis-meta");

        LOGGED.lock().clear();
        let tree = loader.tree_handle();
        let entry = tree.create(
            opts(
                "cordis-meta",
                serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 7").unwrap(),
            ),
            None,
            0,
        );
        tree.await_tree().await;
        let fiber = entry.fiber.lock().clone().expect("fiber created");
        assert_eq!(fiber.state(), FiberState::Active);
        assert!(
            fiber.inject.lock().contains_key("logger"),
            "metadata inject must flow into the fiber"
        );
        assert!(
            LOGGED
                .lock()
                .iter()
                .any(|line| line.contains("meta applied with value 7")),
            "apply must run through the FFI bridge: {:?}",
            LOGGED.lock()
        );
        // Spawn: the apply-spawned task is driven by the host runtime.
        for _ in 0..500 {
            if LOGGED
                .lock()
                .iter()
                .any(|line| line.contains("meta spawned task ran (config value 7)"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            LOGGED
                .lock()
                .iter()
                .any(|line| line.contains("meta spawned task ran")),
            "apply-spawned task must run on the host runtime: {:?}",
            LOGGED.lock()
        );

        // `!expr` configs are evaluated before they reach the `.so` apply.
        // SAFETY: the test is single-threaded (current_thread runtime).
        unsafe { std::env::set_var("CORDIS_META_EXPR", "11") };
        LOGGED.lock().clear();
        let raw = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(
            "value: !expr env(\"CORDIS_META_EXPR\") or 1",
        )
        .unwrap();
        // The loader evaluates `!expr` before the config reaches plugin apply
        // (config-expression integration point).
        let evaluated = evaluate_config(
            &raw,
            &MinijinjaEvaluator,
            &EvalEnv {
                platform: "darwin".to_string(),
                base_url: "https://example.com".to_string(),
                env_vars: std::env::vars().collect(),
            },
        )
        .expect("evaluable");
        let expr_entry = tree.create(opts("cordis-meta", evaluated), None, 0);
        tree.await_tree().await;
        let expr_fiber = expr_entry.fiber.lock().clone().expect("fiber created");
        assert_eq!(expr_fiber.state(), FiberState::Active);
        assert!(
            LOGGED
                .lock()
                .iter()
                .any(|line| line.contains("meta applied with value 11")),
            "!expr value must reach apply: {:?}",
            LOGGED.lock()
        );

        // Invalid configs are rejected by the plugin's validator.
        LOGGED.lock().clear();
        let bad = tree.create(
            opts(
                "cordis-meta",
                serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 0").unwrap(),
            ),
            None,
            0,
        );
        tree.await_tree().await;
        let bad_fiber = bad.fiber.lock().clone().expect("fiber created");
        assert_eq!(bad_fiber.state(), FiberState::Failed);
        assert!(
            !LOGGED
                .lock()
                .iter()
                .any(|line| line.contains("meta applied")),
            "rejected config must not reach apply: {:?}",
            LOGGED.lock()
        );
    }
    .await;
}

/// The same `.so` plugin drives two entries with independent configs and
/// per-instance state.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn same_plugin_two_entries_independent() {
    let _guard = FIXTURE_LOCK.lock();
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
        let handle = unsafe { plugin.create(log_message) };
        assert!(!handle.is_null());
        loader.register_so_plugin(&plugin).expect("register");

        LOGGED.lock().clear();
        let tree = loader.tree_handle();
        let first = tree.create(
            opts(
                "cordis-meta",
                serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 21").unwrap(),
            ),
            None,
            0,
        );
        let second = tree.create(
            opts(
                "cordis-meta",
                serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 22").unwrap(),
            ),
            None,
            0,
        );
        tree.await_tree().await;

        let f1 = first.fiber.lock().clone().expect("fiber");
        let f2 = second.fiber.lock().clone().expect("fiber");
        assert_eq!(f1.state(), FiberState::Active);
        assert_eq!(f2.state(), FiberState::Active);
        assert!(
            !Arc::ptr_eq(&f1, &f2),
            "two entries must have independent fibers"
        );
        let logged = LOGGED.lock().clone();
        assert!(
            logged
                .iter()
                .any(|line| line.contains("meta applied with value 21")),
            "missing 21"
        );
        assert!(
            logged
                .iter()
                .any(|line| line.contains("meta applied with value 22")),
            "missing 22"
        );
        assert_eq!(
            logged
                .iter()
                .filter(|line| line.contains("meta applied with value 2"))
                .count(),
            2,
            "each entry applies with its own config"
        );
    }
    .await;
}

#[allow(dead_code)]
fn _effect_type_check(_: Effect) {}
