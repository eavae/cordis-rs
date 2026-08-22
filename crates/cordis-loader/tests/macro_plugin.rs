//! End-to-end: a plugin written with the `cordis_plugin!` macro loads
//! through the host loader, validates its config and provides services.

use std::collections::HashMap;
use std::path::PathBuf;

use cordis_core::{Context, FiberState};
use cordis_loader::{EntryOptions, Loader, SoPlugin};

fn fixture_path() -> PathBuf {
    // `cargo test` does not emit fixture cdylibs; build them on demand.
    cordis_fixture_builder::ensure_fixtures(&["cordis-fixture-macro"]);
    let mut path = cordis_fixture_builder::artifact_dir();
    #[cfg(target_os = "macos")]
    let file = "libcordis_fixture_macro.dylib";
    #[cfg(target_os = "linux")]
    let file = "libcordis_fixture_macro.so";
    path.push(file);
    path
}

extern "C" fn log_message(_message: *const std::ffi::c_char) {}

fn opts(greeting: &str) -> EntryOptions {
    EntryOptions {
        id: String::new(),
        name: "cordis-macro".to_string(),
        config: Some(
            serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&format!("greeting: {greeting}"))
                .unwrap(),
        ),
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

/// The macro plugin validates its config (an empty greeting is rejected by
/// `plugin_validate_config`) and provides the configured greeting service on
/// a valid config.
#[tokio::test(flavor = "current_thread")]
async fn macro_plugin_validates_config_and_provides() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        // SAFETY: the fixture is built by the workspace and used on one thread.
        let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
        let handle = unsafe { plugin.create(log_message) };
        assert!(!handle.is_null());
        loader.register_so_plugin(&plugin).expect("register");

        let tree = loader.tree_handle();
        let bad = tree.create(opts(""), None, 0);
        tree.await_tree().await;
        let bad_fiber = bad.fiber.lock().clone().expect("fiber created");
        assert_eq!(
            bad_fiber.state(),
            FiberState::Failed,
            "empty greeting must be rejected by plugin_validate_config"
        );

        let entry = tree.create(opts("hi"), None, 0);
        tree.await_tree().await;
        let fiber = entry.fiber.lock().clone().expect("fiber created");
        assert_eq!(fiber.state(), FiberState::Active);

        let greeting = entry
            .ctx
            .get_str("greeting")
            .expect("plugin must provide greeting")
            .downcast_ref::<serde_yaml_ng::Value>()
            .cloned()
            .expect("greeting is a serde value");
        assert_eq!(greeting.as_str(), Some("hi"));
    }
    .await;
}
