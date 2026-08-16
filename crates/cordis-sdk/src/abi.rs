//! The `.so` plugin ABI protocol (story card E2).
//!
//! Hand-written `extern "C"` entry points; cross-boundary objects are opaque
//! handles allocated by the plugin side, and allocation never crosses the
//! boundary.

use std::ffi::c_char;

/// The ABI version implemented by this SDK.
pub const PLUGIN_API_VERSION: u32 = 1;

/// Host callbacks the plugin can use.
#[repr(C)]
pub struct HostVtable {
    /// Logs a message through the host logger.
    pub log: extern "C" fn(message: *const c_char),
    /// The host ABI version (validated by the plugin).
    pub host_version: u32,
}

/// An opaque plugin handle owned by the plugin.
#[repr(C)]
pub struct PluginHandle {
    _private: [u8; 0],
}

/// Returns the plugin ABI version the host must match.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_api_version() -> u32 {
    PLUGIN_API_VERSION
}

/// Creates a plugin instance bound to the host vtable.
///
/// Returns a null pointer on version mismatch or invalid vtable.
///
/// # Safety
///
/// `host` must point to a valid, NUL-free [`HostVtable`] whose function
/// pointers are callable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_create(host: *const HostVtable) -> *mut PluginHandle {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller promises a valid vtable; we only read the version.
    let version = unsafe { (*host).host_version };
    if version != PLUGIN_API_VERSION {
        return std::ptr::null_mut();
    }
    std::ptr::NonNull::<PluginHandle>::dangling().as_ptr()
}

/// Tears down a plugin instance (no-op for the E2 protocol).
///
/// # Safety
///
/// `handle` must come from a matching [`plugin_create`] call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(_handle: *mut PluginHandle) {}
