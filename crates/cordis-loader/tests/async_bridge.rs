//! Cross-FFI async bridge (host drives plugin futures).

use std::path::PathBuf;
use std::sync::Mutex;

use cordis_loader::SoPlugin;
use cordis_sdk::PLUGIN_API_VERSION;
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn fixture_path() -> PathBuf {
    let mut path = std::env::current_dir().unwrap();
    path.push("..");
    path.push("..");
    path.push("target");
    path.push("debug");
    #[cfg(target_os = "macos")]
    let file = "libcordis_fixture_spawn.dylib";
    #[cfg(target_os = "linux")]
    let file = "libcordis_fixture_spawn.so";
    path.push(file);
    path
}

extern "C" fn log_message(message: *const std::ffi::c_char) {
    // SAFETY: the plugin passes a NUL-terminated string.
    let text = unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .to_string();
    LOGGED.lock().unwrap().push(text);
}

async fn wait_logged(needle: &str) {
    for _ in 0..1000 {
        if LOGGED
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle))
        {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "log line {needle:?} never appeared: {:?}",
        LOGGED.lock().unwrap()
    );
}

/// A plugin spawns a future through the host vtable; the host drives it to
/// completion and the result is observable (logged via the vtable).
#[tokio::test(flavor = "current_thread")]
async fn host_drives_spawned_future_to_completion() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());

            LOGGED.lock().unwrap().clear();
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
async fn dispose_cancels_pending_spawns() {
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

/// 10k spawns complete without abnormal growth (smoke).
#[tokio::test(flavor = "current_thread")]
async fn ten_thousand_spawns_smoke() {
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
