//! Cross-FFI async bridge (host drives plugin futures).

use parking_lot::Mutex;
use std::path::PathBuf;

use cordis_loader::SoPlugin;
use cordis_sdk::PLUGIN_API_VERSION;
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The fixture's process-wide counters (`CANCELLED_DROPS`, `COMPLETED`, ...)
/// are shared by every test in this binary; the harness runs tests
/// concurrently, so tests that read them must run serially.
static SERIAL: Mutex<()> = Mutex::new(());

/// Resets the fixture's shared counters (the host keeps the library loaded
/// across tests in this binary, so the counters accumulate otherwise).
fn reset_counters() {
    let library = unsafe { Library::new(fixture_path()) }.unwrap();
    type Reset = unsafe extern "C" fn();
    let reset: libloading::Symbol<Reset> =
        unsafe { library.get(b"plugin_reset_counters") }.unwrap();
    unsafe { reset() };
}

fn fixture_path() -> PathBuf {
    // `cargo test` does not emit fixture cdylibs; build them on demand.
    cordis_fixture_builder::ensure_fixtures(&["cordis-fixture-spawn"]);
    cordis_fixture_builder::artifact_dir().join(if cfg!(target_os = "macos") {
        "libcordis_fixture_spawn.dylib"
    } else {
        "libcordis_fixture_spawn.so"
    })
}

extern "C" fn log_message(message: *const std::ffi::c_char) {
    // SAFETY: the plugin passes a NUL-terminated string.
    let text = unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .to_string();
    LOGGED.lock().push(text);
}

async fn wait_logged(needle: &str) {
    for _ in 0..1000 {
        if LOGGED.lock().iter().any(|line| line.contains(needle)) {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("log line {needle:?} never appeared: {:?}", LOGGED.lock());
}

/// A plugin spawns a future through the host vtable; the host drives it to
/// completion and the result is observable (logged via the vtable).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn host_drives_spawned_future_to_completion() {
    let _serial = SERIAL.lock();
    reset_counters();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());

            LOGGED.lock().clear();
            let library = unsafe { Library::new(fixture_path()) }.unwrap();
            type Spawn = unsafe extern "C" fn(*mut cordis_sdk::PluginHandle);
            let spawn: libloading::Symbol<Spawn> =
                unsafe { library.get(b"plugin_spawn_and_log") }.unwrap();
            unsafe { spawn(handle) };

            wait_logged("spawned task result: 42").await;
            type Count = unsafe extern "C" fn() -> u32;
            let completed: libloading::Symbol<Count> =
                unsafe { library.get(b"plugin_completed") }.unwrap();
            assert_eq!(unsafe { completed() }, 1);
            drop(plugin);
        })
        .await;
}

/// Disposing the plugin handle cancels pending futures (their boxed futures
/// are dropped through the plugin's drop function).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn dispose_cancels_pending_spawns() {
    let _serial = SERIAL.lock();
    reset_counters();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());

            let library = unsafe { Library::new(fixture_path()) }.unwrap();
            type Spawn = unsafe extern "C" fn(*mut cordis_sdk::PluginHandle);
            let spawn: libloading::Symbol<Spawn> =
                unsafe { library.get(b"plugin_spawn_never_completes") }.unwrap();
            unsafe { spawn(handle) };
            tokio::task::yield_now().await;

            type Count = unsafe extern "C" fn() -> u32;
            let cancelled: libloading::Symbol<Count> =
                unsafe { library.get(b"plugin_cancelled_drops") }.unwrap();
            assert_eq!(unsafe { cancelled() }, 0, "task is still pending");

            drop(plugin);
            for _ in 0..100 {
                tokio::task::yield_now().await;
                if unsafe { cancelled() } >= 1 {
                    break;
                }
            }
            assert_eq!(
                unsafe { cancelled() },
                1,
                "disposing the plugin must drop the pending boxed future"
            );
        })
        .await;
}

/// Dropping a plugin while one of its spawned futures is still pending must
/// not unload the library before the host runtime drops the future: the
/// fixture's drop function would otherwise run after `dlclose`, i.e. into
/// unmapped code.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn unload_waits_for_pending_spawned_futures() {
    let _serial = SERIAL.lock();
    reset_counters();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());

            // The extra handle only reaches the fixture's spawn export;
            // release it so the plugin is the sole owner of the library when
            // it is dropped.
            let library = unsafe { Library::new(fixture_path()) }.unwrap();
            type Spawn = unsafe extern "C" fn(*mut cordis_sdk::PluginHandle);
            {
                let spawn: libloading::Symbol<Spawn> =
                    unsafe { library.get(b"plugin_spawn_never_completes") }.unwrap();
                unsafe { spawn(handle) };
                // Let the host poll the new task once so it is registered
                // with the runtime before it is cancelled.
                tokio::task::yield_now().await;
            }
            drop(library);

            drop(plugin);

            // Let the runtime drop the aborted task, then verify through a
            // fresh handle that the fixture's drop function ran: if the
            // library was unloaded and reloaded (current behavior), the new
            // instance's counter never reaches 1, while a pending task must
            // keep the original instance alive until it is dropped.
            let library = unsafe { Library::new(fixture_path()) }.unwrap();
            type Count = unsafe extern "C" fn() -> u32;
            let fresh: libloading::Symbol<Count> =
                unsafe { library.get(b"plugin_cancelled_drops") }.unwrap();
            for _ in 0..100 {
                tokio::task::yield_now().await;
                if unsafe { fresh() } >= 1 {
                    return;
                }
            }
            panic!("pending future was not dropped");
        })
        .await;
}

/// 10k spawns complete without abnormal growth (smoke).
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn ten_thousand_spawns_smoke() {
    let _serial = SERIAL.lock();
    reset_counters();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());

            let library = unsafe { Library::new(fixture_path()) }.unwrap();
            type Spawn = unsafe extern "C" fn(*mut cordis_sdk::PluginHandle, u32);
            let spawn: libloading::Symbol<Spawn> =
                unsafe { library.get(b"plugin_spawn_many") }.unwrap();
            unsafe { spawn(handle, 10_000) };

            type Count = unsafe extern "C" fn() -> u32;
            let completed: libloading::Symbol<Count> =
                unsafe { library.get(b"plugin_completed") }.unwrap();
            let baseline = unsafe { completed() };
            for _ in 0..2000 {
                tokio::task::yield_now().await;
                if unsafe { completed() } == baseline + 10_000 {
                    break;
                }
            }
            assert_eq!(unsafe { completed() }, baseline + 10_000);
            drop(plugin);
        })
        .await;
}

/// The vtable is version-checked by the fixture.
#[test]
fn fixture_exports_current_abi_version() {
    let library = unsafe { Library::new(fixture_path()) }.unwrap();
    type ApiVersion = unsafe extern "C" fn() -> u32;
    let version: libloading::Symbol<ApiVersion> =
        unsafe { library.get(b"plugin_api_version") }.unwrap();
    assert_eq!(unsafe { version() }, PLUGIN_API_VERSION);
}
