//! Host-side dynamic loader (`SoPlugin`).
//!
//! Fixture cdylibs are built on demand (`cargo build -p cordis-fixture-*`)
//! before these tests open them, so a plain `cargo test --workspace` works
//! on a clean checkout.

use parking_lot::{Mutex, MutexGuard};
use std::path::PathBuf;

use cordis_loader::{LoadError, SoPlugin, is_plugin_path};
use cordis_sdk::PLUGIN_API_VERSION;
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The fixture dylib keeps process-global state (dispose counter), so tests
/// that observe it must not run concurrently.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn lock_fixture() -> MutexGuard<'static, ()> {
    FIXTURE_LOCK.lock()
}

fn fixture_path(name: &str) -> PathBuf {
    // `cargo test` does not emit fixture cdylibs; build them on demand.
    let package = name.replace('_', "-");
    cordis_fixture_builder::ensure_fixtures(&[package.as_str()]);
    let mut path = cordis_fixture_builder::artifact_dir();
    #[cfg(target_os = "macos")]
    let file = format!("lib{name}.dylib");
    #[cfg(target_os = "linux")]
    let file = format!("lib{name}.so");
    path.push(file);
    path
}

extern "C" fn log_message(message: *const std::ffi::c_char) {
    // SAFETY: the plugin passes a NUL-terminated string.
    let text = unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .to_string();
    LOGGED.lock().push(text);
}

/// Loading succeeds, `create` returns a callable handle and the lifecycle
/// hooks fire through the host vtable.
#[test]
fn load_create_and_drop_disposes() {
    let _guard = lock_fixture();
    let path = fixture_path("cordis_fixture_hello");
    // Keep the image mapped after `SoPlugin`'s dlclose so the fixture's
    // process-global counter is still visible to `dispose_count` (Linux
    // unloads the image at refcount 0; macOS dyld retains it).
    let _keepalive = unsafe { Library::new(&path) }.unwrap();
    // SAFETY: the fixture is built by the workspace and used on one thread.
    let mut plugin = unsafe { SoPlugin::load(&path) }.expect("fixture must load");
    assert_eq!(plugin.version(), PLUGIN_API_VERSION);
    assert_eq!(plugin.path(), path);

    LOGGED.lock().clear();
    // SAFETY: `vtable` outlives the call and the fixture expects it.
    let handle = unsafe { plugin.create(log_message) };
    assert!(!handle.is_null(), "plugin_create must return a handle");
    assert_eq!(
        LOGGED.lock().as_slice(),
        &["hello from fixture plugin".to_string()]
    );

    let before = dispose_count(&path);
    drop(plugin);
    assert_eq!(
        dispose_count(&path),
        before + 1,
        "dropping SoPlugin must call plugin_dispose"
    );
}

fn dispose_count(path: &std::path::Path) -> u32 {
    // SAFETY: the fixture library is already loaded by the process.
    let library = unsafe { Library::new(path) }.unwrap();
    type Count = unsafe extern "C" fn() -> u32;
    // SAFETY: symbol exported by the fixture.
    let count: libloading::Symbol<Count> = unsafe { library.get(b"plugin_dispose_count") }.unwrap();
    unsafe { count() }
}

/// Loading the same file twice yields independent handles.
#[test]
fn repeated_loads_are_independent() {
    let _guard = lock_fixture();
    let path = fixture_path("cordis_fixture_hello");
    // Keep the image mapped across the drops so `dispose_count` reopens the
    // same instance (Linux unloads the image at refcount 0).
    let _keepalive = unsafe { Library::new(&path) }.unwrap();
    // SAFETY: each plugin is used on one thread.
    let mut first = unsafe { SoPlugin::load(&path) }.unwrap();
    let mut second = unsafe { SoPlugin::load(&path) }.unwrap();
    let before = dispose_count(&path);
    // SAFETY: vtables outlive the calls.
    let a = unsafe { first.create(log_message) };
    let b = unsafe { second.create(log_message) };
    assert!(!a.is_null() && !b.is_null());
    drop(first);
    assert_eq!(dispose_count(&path), before + 1, "one drop → one dispose");
    drop(second);
    assert_eq!(dispose_count(&path), before + 2);
}

/// A non-existent file produces an `Open` error carrying the path.
#[test]
fn missing_file_error_includes_path() {
    let _guard = lock_fixture();
    let missing = std::env::temp_dir().join("cordis-no-such-plugin.dylib");
    // SAFETY: we never touch the returned error's library.
    let error = match unsafe { SoPlugin::load(&missing) } {
        Ok(_) => panic!("loading a missing file must fail"),
        Err(error) => error,
    };
    assert!(matches!(error, LoadError::Open { .. }));
    let text = error.to_string();
    assert!(text.contains(&missing.display().to_string()), "{text}");
}

/// An unsupported ABI version is rejected with a dedicated error.
#[test]
fn version_mismatch_is_rejected() {
    let _guard = lock_fixture();
    let path = fixture_path("cordis_fixture_bad_version");
    // SAFETY: fixture used on one thread.
    let error = match unsafe { SoPlugin::load(&path) } {
        Ok(_) => panic!("version mismatch must fail"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        LoadError::VersionMismatch {
            found,
            expected: PLUGIN_API_VERSION,
            ..
        } if found == PLUGIN_API_VERSION + 1
    ));
    assert!(
        error
            .to_string()
            .contains(&format!("exports ABI version {}", PLUGIN_API_VERSION + 1))
    );
}

/// A loadable library that is not a Cordis plugin is rejected with a
/// `MissingSymbol` error naming the missing symbol — the Rust counterpart of
/// the JS "invalid plugin" shape check at the dynamic boundary.
#[test]
fn missing_symbol_is_rejected() {
    let _guard = lock_fixture();
    let path = fixture_path("cordis_fixture_not_a_plugin");
    // SAFETY: fixture used on one thread.
    let error = match unsafe { SoPlugin::load(&path) } {
        Ok(_) => panic!("a non-plugin library must fail to load"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        LoadError::MissingSymbol {
            symbol: "plugin_api_version",
            ..
        }
    ));
    let text = error.to_string();
    assert!(text.contains("missing symbol plugin_api_version"), "{text}");
    assert!(text.contains("cordis_fixture_not_a_plugin"), "{text}");
}

/// Name classification routes `cordis:` builtins vs native paths.
#[test]
fn plugin_name_classification() {
    assert!(!is_plugin_path("cordis:logger"));
    assert!(!is_plugin_path("@cordisjs/plugin-group"));
    assert!(is_plugin_path("plugins/foo.so"));
    #[cfg(target_os = "macos")]
    assert!(is_plugin_path("plugins/foo.dylib"));
    #[cfg(target_os = "windows")]
    assert!(is_plugin_path(r"plugins\foo.dll"));
    #[cfg(not(target_os = "windows"))]
    assert!(is_plugin_path("foo.dll"));
}
