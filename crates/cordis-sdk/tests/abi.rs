//! ABI smoke tests (host loads fixture `.so` files).

use parking_lot::Mutex;
use std::ffi::c_char;
use std::path::PathBuf;

use cordis_sdk::{FfiFuture, HostVtable, PLUGIN_API_VERSION, PluginHandle};
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

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

type ApiVersion = unsafe extern "C" fn() -> u32;
type Create = unsafe extern "C" fn(*const HostVtable) -> *mut PluginHandle;
type Dispose = unsafe extern "C" fn(*mut PluginHandle);
type ValidateConfig = unsafe extern "C" fn(*const c_char) -> i32;
type ApplyConfig = unsafe extern "C" fn(*mut PluginHandle, *const c_char) -> i32;
type Count = extern "C" fn() -> u32;

static PROVIDED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

unsafe extern "C" fn capture_provide(
    _handle: *mut PluginHandle,
    name: *const c_char,
    payload: *const c_char,
) -> i32 {
    // SAFETY: the plugin passes NUL-terminated strings.
    let name = unsafe { std::ffi::CStr::from_ptr(name) }
        .to_string_lossy()
        .to_string();
    let payload = unsafe { std::ffi::CStr::from_ptr(payload) }
        .to_string_lossy()
        .to_string();
    PROVIDED.lock().push((name, payload));
    0
}

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
        LOGGED.lock().push(text);
    }
    let vtable = HostVtable {
        log: log_message,
        spawn: noop_spawn,
        sleep: noop_sleep,
        spawn_blocking: noop_spawn_blocking,
        provide: noop_provide,
        get: noop_get,
        on: noop_on,
        emit: noop_emit,
        effect_disposer: noop_effect_disposer,
        data: std::ptr::null_mut(),
        host_version: PLUGIN_API_VERSION,
    };
    LOGGED.lock().clear();
    let handle = unsafe { create(&vtable) };
    assert!(!handle.is_null(), "plugin_create must succeed");
    assert_eq!(
        LOGGED.lock().as_slice(),
        &["hello from fixture plugin".to_string()]
    );
    unsafe { dispose(handle) };
}

unsafe extern "C" fn noop_spawn(_data: *mut std::ffi::c_void, _future: FfiFuture<()>) {}

extern "C" fn noop_sleep(_data: *mut std::ffi::c_void, _millis: u64) -> FfiFuture<()> {
    use async_ffi::FutureExt;
    async {}.into_ffi()
}

unsafe extern "C" fn noop_spawn_blocking(
    _data: *mut std::ffi::c_void,
    _work: unsafe extern "C" fn(*mut std::ffi::c_void),
    _arg: *mut std::ffi::c_void,
) {
}

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
        sleep: noop_sleep,
        spawn_blocking: noop_spawn_blocking,
        provide: noop_provide,
        get: noop_get,
        on: noop_on,
        emit: noop_emit,
        effect_disposer: noop_effect_disposer,
        data: std::ptr::null_mut(),
        host_version: PLUGIN_API_VERSION,
    };
    let handle = unsafe { create(&vtable) };
    assert!(
        handle.is_null(),
        "host must reject a plugin with an unsupported version"
    );
}

/// A plugin written with the `cordis_plugin!` macro exports the full ABI and
/// routes config parsing, validation and apply correctly.
#[test]
fn macro_plugin_exports_abi_and_applies() {
    // SAFETY: the fixture library is built by the workspace before tests.
    let library = unsafe { Library::new(fixture_path("cordis_fixture_macro")) }.unwrap();
    let version: libloading::Symbol<ApiVersion> =
        unsafe { library.get(b"plugin_api_version") }.unwrap();
    assert_eq!(unsafe { version() }, PLUGIN_API_VERSION);

    let create: libloading::Symbol<Create> = unsafe { library.get(b"plugin_create") }.unwrap();
    let dispose: libloading::Symbol<Dispose> = unsafe { library.get(b"plugin_dispose") }.unwrap();
    let validate: libloading::Symbol<ValidateConfig> =
        unsafe { library.get(b"plugin_validate_config") }.unwrap();
    let apply: libloading::Symbol<ApplyConfig> = unsafe { library.get(b"plugin_apply") }.unwrap();
    let apply_count: libloading::Symbol<Count> =
        unsafe { library.get(b"macro_apply_count") }.unwrap();
    let validate_count: libloading::Symbol<Count> =
        unsafe { library.get(b"macro_validate_count") }.unwrap();

    let vtable = HostVtable {
        log: noop_log,
        spawn: noop_spawn,
        sleep: noop_sleep,
        spawn_blocking: noop_spawn_blocking,
        provide: capture_provide,
        get: noop_get,
        on: noop_on,
        emit: noop_emit,
        effect_disposer: noop_effect_disposer,
        data: std::ptr::null_mut(),
        host_version: PLUGIN_API_VERSION,
    };
    let handle = unsafe { create(&vtable) };
    assert!(!handle.is_null(), "macro plugin_create must succeed");

    // Bad JSON fails at parse; an empty greeting fails the user validator; a
    // valid config passes.
    assert_eq!(unsafe { validate(c"not json".as_ptr()) }, 1);
    assert_eq!(unsafe { validate(c"{\"greeting\":\"\"}".as_ptr()) }, 1);
    assert_eq!(unsafe { validate(c"{\"greeting\":\"hi\"}".as_ptr()) }, 0);

    PROVIDED.lock().clear();
    assert_eq!(
        unsafe { apply(handle, c"{\"greeting\":\"hi\"}".as_ptr()) },
        0
    );
    assert_eq!(
        PROVIDED.lock().as_slice(),
        &[("greeting".to_string(), "\"hi\"".to_string())]
    );
    assert_eq!(apply_count(), 1);
    assert_eq!(validate_count(), 2);

    unsafe { dispose(handle) };
}

extern "C" fn noop_log(_message: *const c_char) {}

unsafe extern "C" fn noop_provide(
    _handle: *mut PluginHandle,
    _name: *const c_char,
    _payload: *const c_char,
) -> i32 {
    0
}

unsafe extern "C" fn noop_get(_handle: *mut PluginHandle, _name: *const c_char) -> *const c_char {
    std::ptr::null()
}

unsafe extern "C" fn noop_on(
    _handle: *mut PluginHandle,
    _event: *const c_char,
    _callback: cordis_sdk::PluginEventCallback,
) -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

unsafe extern "C" fn noop_emit(
    _handle: *mut PluginHandle,
    _event: *const c_char,
    _payload: *const c_char,
) {
}

unsafe extern "C" fn noop_effect_disposer(
    _handle: *mut PluginHandle,
    _disposer: cordis_sdk::PluginDisposer,
) {
}
