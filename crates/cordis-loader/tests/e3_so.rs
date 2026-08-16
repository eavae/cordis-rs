//! Story card E3: host-side dynamic loader (`SoPlugin`).
//!
//! Fixtures are workspace cdylib members, so `cargo test --workspace`
//! builds `libcordis_fixture_*.dylib` / `libcordis_fixture_*.so` into
//! `target/debug` before these tests run.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use cordis_loader::{LoadError, SoPlugin, is_plugin_path};
use cordis_sdk::{HostVtable, PLUGIN_API_VERSION};
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The fixture dylib keeps process-global state (dispose counter), so tests
/// that observe it must not run concurrently.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn lock_fixture() -> MutexGuard<'static, ()> {
    FIXTURE_LOCK.lock().unwrap()
}

fn fixture_path(name: &str) -> PathBuf {
    let mut path = std::env::current_dir().unwrap();
    path.push("..");
    path.push("..");
    path.push("target");
    path.push("debug");
    #[cfg(target_os = "macos")]
    let file = format!("lib{name}.dylib");
    #[cfg(target_os = "linux")]
    let file = format!("lib{name}.so");
    path.push(file);
    path
}

fn vtable() -> HostVtable {
    extern "C" fn log_message(message: *const std::ffi::c_char) {
        // SAFETY: the plugin passes a NUL-terminated string.
        let text = unsafe { std::ffi::CStr::from_ptr(message) }
            .to_string_lossy()
            .to_string();
        LOGGED.lock().unwrap().push(text);
    }
    HostVtable {
        log: log_message,
        host_version: PLUGIN_API_VERSION,
    }
}

/// E3.1: loading succeeds, `create` returns a callable handle and the
/// lifecycle hooks fire through the host vtable.
#[test]
fn load_create_and_drop_disposes() {
    let _guard = lock_fixture();
    let path = fixture_path("cordis_fixture_hello");
    // SAFETY: the fixture is built by the workspace and used on one thread.
    let mut plugin = unsafe { SoPlugin::load(&path) }.expect("fixture must load");
    assert_eq!(plugin.version(), PLUGIN_API_VERSION);
    assert_eq!(plugin.path(), path);

    LOGGED.lock().unwrap().clear();
    // SAFETY: `vtable` outlives the call and the fixture expects it.
    let handle = unsafe { plugin.create(&vtable()) };
    assert!(!handle.is_null(), "plugin_create must return a handle");
    assert_eq!(
        LOGGED.lock().unwrap().as_slice(),
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

/// E3.2: loading the same file twice yields independent handles.
#[test]
fn repeated_loads_are_independent() {
    let _guard = lock_fixture();
    let path = fixture_path("cordis_fixture_hello");
    // SAFETY: each plugin is used on one thread.
    let mut first = unsafe { SoPlugin::load(&path) }.unwrap();
    let mut second = unsafe { SoPlugin::load(&path) }.unwrap();
    let before = dispose_count(&path);
    // SAFETY: vtables outlive the calls.
    let a = unsafe { first.create(&vtable()) };
    let b = unsafe { second.create(&vtable()) };
    assert!(!a.is_null() && !b.is_null());
    drop(first);
    assert_eq!(dispose_count(&path), before + 1, "one drop → one dispose");
    drop(second);
    assert_eq!(dispose_count(&path), before + 2);
}

/// E3.3: a non-existent file produces an `Open` error carrying the path.
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

/// E3.3: an unsupported ABI version is rejected with a dedicated error.
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
            found: 2,
            expected: PLUGIN_API_VERSION,
            ..
        }
    ));
    assert!(error.to_string().contains("exports ABI version 2"));
}

/// E3.5: name classification routes `cordis:` builtins vs native paths.
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
