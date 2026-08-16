//! A fixture exporting an unsupported ABI version (E2 version-mismatch test).

use cordis_sdk::{HostVtable, PluginHandle};

#[unsafe(no_mangle)]
pub extern "C" fn plugin_api_version() -> u32 {
    2
}

/// # Safety
///
/// `host` must point to a valid vtable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_create(_host: *const HostVtable) -> *mut PluginHandle {
    std::ptr::null_mut()
}

/// # Safety
///
/// `handle` must come from a matching create call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(_handle: *mut PluginHandle) {}
