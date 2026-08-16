//! Story card E2: ABI smoke tests (host loads fixture `.so` files).

use std::ffi::c_char;
use std::path::PathBuf;
use std::sync::Mutex;

use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle};
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

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

type ApiVersion = unsafe extern "C" fn() -> u32;
type Create = unsafe extern "C" fn(*const HostVtable) -> *mut PluginHandle;
type Dispose = unsafe extern "C" fn(*mut PluginHandle);

#[test]
fn host_loads_fixture_and_round_trips() {
    // SAFETY: the fixture library is built by the workspace before tests.
    let library = unsafe { Library::new(fixture_path("cordis_fixture_hello")) }.unwrap();
    let version: libloading::Symbol<ApiVersion> =
        unsafe { library.get(b"plugin_api_version") }.unwrap();
    assert_eq!(unsafe { version() }, PLUGIN_API_VERSION);

    let create: libloading::Symbol<Create> = unsafe { library.get(b"plugin_create") }.unwrap();
    let dispose: libloading::Symbol<Dispose> = unsafe { library.get(b"plugin_dispose") }.unwrap();

    extern "C" fn log_message(message: *const c_char) {
        // SAFETY: the plugin passes a NUL-terminated string.
        let text = unsafe { std::ffi::CStr::from_ptr(message) }
            .to_string_lossy()
            .to_string();
        LOGGED.lock().unwrap().push(text);
    }
    let vtable = HostVtable {
        log: log_message,
        spawn: noop_spawn,
        data: std::ptr::null_mut(),
        host_version: PLUGIN_API_VERSION,
    };
    LOGGED.lock().unwrap().clear();
    let handle = unsafe { create(&vtable) };
    assert!(!handle.is_null(), "plugin_create must succeed");
    assert_eq!(
        LOGGED.lock().unwrap().as_slice(),
        &["hello from fixture plugin".to_string()]
    );
    unsafe { dispose(handle) };
}

unsafe extern "C" fn noop_spawn(_data: *mut std::ffi::c_void, _future: *mut std::ffi::c_void) {}

#[test]
fn host_rejects_version_mismatch() {
    // SAFETY: fixture built by the workspace.
    let library = unsafe { Library::new(fixture_path("cordis_fixture_bad_version")) }.unwrap();
    let version: libloading::Symbol<ApiVersion> =
        unsafe { library.get(b"plugin_api_version") }.unwrap();
    assert_ne!(
        unsafe { version() },
        PLUGIN_API_VERSION,
        "fixture must export a different version"
    );

    let create: libloading::Symbol<Create> = unsafe { library.get(b"plugin_create") }.unwrap();
    let vtable = HostVtable {
        log: noop_log,
        spawn: noop_spawn,
        data: std::ptr::null_mut(),
        host_version: PLUGIN_API_VERSION,
    };
    let handle = unsafe { create(&vtable) };
    assert!(
        handle.is_null(),
        "host must reject a plugin with an unsupported version"
    );
}

extern "C" fn noop_log(_message: *const c_char) {}
