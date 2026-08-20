//! `.so` reload semantics — dispose the old instance, load a new one;
//! per-instance state does not survive.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use cordis_core::{Context, FiberState};
use cordis_loader::{EntryOptions, Loader, SoPlugin};

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
    LOGGED.lock().unwrap().push(text);
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

/// Dispose the old instance, then load a fresh one from the same file: the
/// new instance is clean (state does not survive the reload).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn reload_gives_clean_instance() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // First instance: apply once → per-instance counter is 1.
            let mut first = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { first.create(log_message) };
            assert!(!handle.is_null());
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.register_so_plugin(&first).expect("register");
            let tree = loader.tree_handle();
            tree.create(
                opts(
                    "cordis-meta",
                    serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 3").unwrap(),
                ),
                None,
                0,
            );
            tree.await_tree().await;
            let library = unsafe { libloading::Library::new(fixture_path()) }.unwrap();
            type Count = unsafe extern "C" fn(*mut cordis_sdk::PluginHandle) -> u32;
            let count: libloading::Symbol<Count> =
                unsafe { library.get(b"plugin_apply_count") }.unwrap();
            assert_eq!(unsafe { count(handle) }, 1);

            // "Reload": dispose the old instance entirely.
            drop(first);

            // Second instance: fresh state (counter starts at 1 again).
            let mut second = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle2 = unsafe { second.create(log_message) };
            assert!(!handle2.is_null());

            let root2 = Context::new();
            let loader2 = Loader::new(&root2);
            loader2.register_so_plugin(&second).expect("register");
            let tree2 = loader2.tree_handle();
            tree2.create(
                opts(
                    "cordis-meta",
                    serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 5").unwrap(),
                ),
                None,
                0,
            );
            tree2.await_tree().await;
            let count2: libloading::Symbol<Count> =
                unsafe { library.get(b"plugin_apply_count") }.unwrap();
            assert_eq!(
                unsafe { count2(handle2) },
                1,
                "state must not survive the reload"
            );
        })
        .await;
}

/// The old instance's disposer ran (`plugin_dispose` called) and the new
/// instance applies cleanly through the bridge.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn dispose_then_reapply_works() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());

            LOGGED.lock().unwrap().clear();
            drop(plugin); // dispose

            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.register_so_plugin(&plugin).expect("register");
            let tree = loader.tree_handle();
            let entry = tree.create(
                opts(
                    "cordis-meta",
                    serde_yaml_ng::from_str::<serde_yaml_ng::Value>("value: 9").unwrap(),
                ),
                None,
                0,
            );
            tree.await_tree().await;
            let fiber = entry.fiber.borrow().clone().expect("fiber");
            assert_eq!(fiber.state.get(), FiberState::Active);
            assert!(
                LOGGED
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|line| line.contains("meta applied with value 9")),
                "fresh instance must apply: {:?}",
                LOGGED.lock().unwrap()
            );
        })
        .await;
}

#[allow(dead_code)]
fn _state_check(_: FiberState) {}
