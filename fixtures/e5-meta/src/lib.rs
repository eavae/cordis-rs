//! Story cards E5/E6 fixture: metadata, config validation and apply bridge.

use std::ffi::c_char;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_sdk::spawn;
use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle};
use serde::Deserialize;

/// The plugin's config schema (serde + manual validation).
#[derive(Deserialize)]
struct Config {
    /// The payload (a number or a numeric string); must be >= 1.
    value: serde_json::Value,
}

impl Config {
    fn number(&self) -> i64 {
        match &self.value {
            serde_json::Value::Number(number) => number.as_i64().unwrap_or(0),
            serde_json::Value::String(text) => text.parse::<i64>().unwrap_or(0),
            _ => 0,
        }
    }
}

/// A plugin instance: keeps the host vtable alive for the handle's lifetime.
struct PluginInstance {
    vtable: *const HostVtable,
    /// Per-instance state: does not survive reload (E7).
    apply_count: AtomicU32,
}

const META: &std::ffi::CStr =
    c"{\"name\":\"cordis-e5-meta\",\"version\":\"1.0.0\",\"inject\":[\"logger\"],\"provide\":[\"greeting\"]}";

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
    // SAFETY: the caller promises a valid vtable; we only read the version.
    let version = unsafe { (*host).host_version };
    if version != PLUGIN_API_VERSION {
        return std::ptr::null_mut();
    }
    let instance = Box::new(PluginInstance {
        vtable: host,
        apply_count: AtomicU32::new(0),
    });
    Box::into_raw(instance).cast::<PluginHandle>()
}

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

/// E6: the metadata payload (JSON, NUL-terminated, static).
#[unsafe(no_mangle)]
pub extern "C" fn plugin_meta() -> *const c_char {
    META.as_ptr()
}

/// E5: validates a config JSON string; 0 = valid, non-zero = invalid.
///
/// # Safety
///
/// `config` must be a NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_validate_config(config: *const c_char) -> i32 {
    // SAFETY: the caller passes a NUL-terminated string.
    let raw = unsafe { std::ffi::CStr::from_ptr(config) }.to_string_lossy();
    match serde_json::from_str::<Config>(&raw) {
        Ok(config) if config.number() >= 1 => 0,
        _ => 1,
    }
}

/// E5: applies a config JSON string through the host vtable.
///
/// # Safety
///
/// `handle` must come from `plugin_create`; `config` must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_apply(handle: *mut PluginHandle, config: *const c_char) -> i32 {
    // SAFETY: the handle came from plugin_create and is alive.
    let instance = unsafe { &*(handle as *mut PluginInstance) };
    instance.apply_count.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the host vtable outlives the plugin instance.
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the caller passes a NUL-terminated string.
    let raw = unsafe { std::ffi::CStr::from_ptr(config) }.to_string_lossy();
    let config = serde_json::from_str::<Config>(&raw).unwrap_or(Config {
        value: serde_json::Value::Number(0.into()),
    });
    let value = config.number();
    let message = std::ffi::CString::new(format!("e5 applied with value {value}")).expect("no NUL");
    (vtable.log)(message.as_ptr());
    // E8: spawn an async task from within apply; the host drives it.
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable: &'static HostVtable = unsafe { std::mem::transmute(vtable) };
    drop(unsafe {
        spawn(vtable, async move {
            let message =
                std::ffi::CString::new(format!("e5 spawned task ran (config value {value})"))
                    .expect("no NUL");
            (vtable.log)(message.as_ptr());
        })
    });
    0
}

/// E7: per-instance apply count (fresh after reload).
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_apply_count(handle: *mut PluginHandle) -> u32 {
    // SAFETY: the handle came from plugin_create and is alive.
    let instance = unsafe { &*(handle as *mut PluginInstance) };
    instance.apply_count.load(Ordering::SeqCst)
}
