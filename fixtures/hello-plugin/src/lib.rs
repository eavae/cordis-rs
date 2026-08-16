//! A minimal `.so` fixture plugin for the E2 ABI smoke test.

use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle};
use std::sync::atomic::{AtomicU32, Ordering};

/// Number of times `plugin_dispose` has been called in this process.
static DISPOSE_COUNT: AtomicU32 = AtomicU32::new(0);

/// The entry points are exported by the host loader.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_api_version() -> u32 {
    PLUGIN_API_VERSION
}

/// # Safety
///
/// `host` must point to a valid vtable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_create(host: *const HostVtable) -> *mut PluginHandle {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // Log through the host vtable.
    let message = c"hello from fixture plugin";
    let log = unsafe { (*host).log };
    // SAFETY: the vtable contract guarantees a valid log function pointer.
    log(message.as_ptr());
    std::ptr::NonNull::<PluginHandle>::dangling().as_ptr()
}

/// # Safety
///
/// `handle` must come from a matching create call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(_handle: *mut PluginHandle) {
    DISPOSE_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Host-side test helper: how many times `plugin_dispose` was called.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_dispose_count() -> u32 {
    DISPOSE_COUNT.load(Ordering::SeqCst)
}
