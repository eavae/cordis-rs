//! Fixture exercising the host async bridge through the ABI.

use std::sync::atomic::{AtomicU32, Ordering};

use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle, spawn};

/// Number of times `plugin_create` has been called.
static CREATE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Number of spawned tasks whose boxed future was dropped before completing.
static CANCELLED_DROPS: AtomicU32 = AtomicU32::new(0);
/// Number of completed spawned tasks.
static COMPLETED: AtomicU32 = AtomicU32::new(0);

/// A plugin instance: keeps the host vtable alive for the handle's lifetime.
struct PluginInstance {
    vtable: *const HostVtable,
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_api_version() -> u32 {
    PLUGIN_API_VERSION
}

/// # Safety
///
/// `host` must point to a valid vtable that outlives the plugin instance.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_create(host: *const HostVtable) -> *mut PluginHandle {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    CREATE_COUNT.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the caller promises a valid vtable; we only read the version.
    let version = unsafe { (*host).host_version };
    if version != PLUGIN_API_VERSION {
        return std::ptr::null_mut();
    }
    let instance = Box::new(PluginInstance { vtable: host });
    Box::into_raw(instance).cast::<PluginHandle>()
}

/// # Safety
///
/// `handle` must come from a matching create call.
/// # Safety
///
/// `handle` must come from a matching create call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(handle: *mut PluginHandle) {
    if handle.is_null() {
        return;
    }
    // SAFETY: the handle came from plugin_create.
    drop(unsafe { Box::from_raw(handle as *mut PluginInstance) });
}

unsafe fn instance(handle: *mut PluginHandle) -> &'static PluginInstance {
    // SAFETY: the handle came from plugin_create and is alive until dispose.
    unsafe { &*(handle as *mut PluginInstance) }
}

/// Spawns a task that logs its result through the host vtable on completion.
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_spawn_and_log(handle: *mut PluginHandle) {
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the vtable is the one the host provided.
    let vtable: &'static HostVtable = unsafe { std::mem::transmute(vtable) };
    // SAFETY: the vtable is the one the host provided.
    drop(unsafe {
        spawn(vtable, async move {
            let message = std::ffi::CString::new("spawned task result: 42").unwrap();
            (vtable.log)(message.as_ptr());
            COMPLETED.fetch_add(1, Ordering::SeqCst);
            42u32
        })
    });
}

/// Spawns a task that never completes; the host cancels its boxed future
/// when the plugin instance is disposed.
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_spawn_never_completes(handle: *mut PluginHandle) {
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the vtable is the one the host provided.
    drop(unsafe {
        spawn(vtable, async {
            let guard = CancelledGuard;
            std::future::pending::<()>().await;
            let _ = guard;
        })
    });
}

/// Spawns `count` short-lived tasks without awaiting them (smoke test).
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_spawn_many(handle: *mut PluginHandle, count: u32) {
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable = unsafe { &*instance.vtable };
    for index in 0..count {
        // SAFETY: the vtable is the one the host provided.
        drop(unsafe {
            spawn(vtable, async move {
                COMPLETED.fetch_add(1, Ordering::SeqCst);
                index
            })
        });
    }
}

/// Host test helpers.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_create_count() -> u32 {
    CREATE_COUNT.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_cancelled_drops() -> u32 {
    CANCELLED_DROPS.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_completed() -> u32 {
    COMPLETED.load(Ordering::SeqCst)
}

/// Counts drops of a never-completing future (cancellation detection).
struct CancelledGuard;

impl Drop for CancelledGuard {
    fn drop(&mut self) {
        CANCELLED_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
