//! Fixture plugin: the Context bridge across FFI.
//!
//! Exercises every vtable entry the host exposes for services, events and
//! fiber-bound disposers: `provide`/`get`, `on`/`emit` and
//! `effect_disposer`.

#![allow(missing_docs)]

use std::ffi::{CStr, CString, c_char};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use cordis_sdk::{ContextBridge, HostVtable, PLUGIN_API_VERSION, PluginHandle};
use serde::Deserialize;

static APPLY_COUNT: AtomicU32 = AtomicU32::new(0);
static EVENT_COUNT: AtomicU32 = AtomicU32::new(0);
/// How many disposers have run (0-based run order).
static DISPOSER_RUNS: AtomicU32 = AtomicU32::new(0);
/// Run order of the disposer registered first (-1 = not run).
static FIRST_ORDER: AtomicI32 = AtomicI32::new(-1);
/// Run order of the disposer registered second (-1 = not run).
static SECOND_ORDER: AtomicI32 = AtomicI32::new(-1);

/// A plugin instance: keeps the host vtable alive for the handle's lifetime.
struct PluginInstance {
    vtable: *const HostVtable,
}

const META: &CStr =
    c"{\"name\":\"cordis-context\",\"version\":\"1.0.0\",\"inject\":[\"logger\"],\"provide\":[\"greeting\"]}";

/// The plugin's config: a greeting string stored as a service.
#[derive(Deserialize)]
struct Config {
    #[serde(default = "default_greeting")]
    greeting: String,
}

fn default_greeting() -> String {
    "hello".to_string()
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

/// The metadata payload (JSON, NUL-terminated, static).
#[unsafe(no_mangle)]
pub extern "C" fn plugin_meta() -> *const c_char {
    META.as_ptr()
}

/// Accepts any object config.
///
/// # Safety
///
/// `config` must be a NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_validate_config(_config: *const c_char) -> i32 {
    0
}

/// Applies through the Context bridge — provides a greeting service,
/// round-trips it through `get`, registers a `demo/event` listener and two
/// fiber-bound disposers.
///
/// # Safety
///
/// `handle` must come from `plugin_create`; `config` must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_apply(handle: *mut PluginHandle, config: *const c_char) -> i32 {
    APPLY_COUNT.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the handle came from plugin_create and is alive.
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance.
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the caller passes a NUL-terminated string.
    let raw = unsafe { CStr::from_ptr(config) }.to_string_lossy();
    let config: Config = serde_json::from_str(&raw).unwrap_or_else(|_| Config {
        greeting: default_greeting(),
    });
    // SAFETY: the vtable is the one the host provided for this handle.
    let bridge = unsafe { ContextBridge::new(vtable, handle) };

    let greeting = config.greeting;
    let payload = serde_json::to_string(&greeting).unwrap_or_else(|_| "\"hello\"".to_string());
    match bridge.provide("greeting", &payload) {
        Ok(()) => log(vtable, format!("context provided greeting: {greeting}")),
        Err(error) => log(vtable, format!("context provide failed: {error}")),
    }
    match bridge.on("demo/event", on_demo_event) {
        Ok(_) => log(
            vtable,
            "context listener registered: demo/event".to_string(),
        ),
        Err(error) => log(vtable, format!("context listener failed: {error}")),
    }
    bridge.effect_disposer(disposer_first);
    bridge.effect_disposer(disposer_second);
    0
}

/// Host test helpers.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_reset() {
    APPLY_COUNT.store(0, Ordering::SeqCst);
    EVENT_COUNT.store(0, Ordering::SeqCst);
    DISPOSER_RUNS.store(0, Ordering::SeqCst);
    FIRST_ORDER.store(-1, Ordering::SeqCst);
    SECOND_ORDER.store(-1, Ordering::SeqCst);
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_apply_count() -> u32 {
    APPLY_COUNT.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_event_count() -> u32 {
    EVENT_COUNT.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_disposer_order_first() -> i32 {
    FIRST_ORDER.load(Ordering::SeqCst)
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_disposer_order_second() -> i32 {
    SECOND_ORDER.load(Ordering::SeqCst)
}

fn log(vtable: &HostVtable, text: String) {
    let message = CString::new(text).expect("log text has no NUL");
    (vtable.log)(message.as_ptr());
}

/// The `demo/event` listener: logs the JSON args and round-trips the
/// greeting service through `get` (the fiber is ACTIVE during event
/// dispatch, so strict lookup resolves the value provided in apply).
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call; `args` must
/// be a NUL-terminated JSON string valid for the call.
unsafe extern "C" fn on_demo_event(handle: *mut PluginHandle, args: *const c_char) {
    EVENT_COUNT.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the host passes a live handle for the current callback.
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance.
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the host passes a NUL-terminated string.
    let text = unsafe { CStr::from_ptr(args) }.to_string_lossy();
    log(vtable, format!("context event fired: {text}"));
    // SAFETY: the vtable is the one the host provided for this handle and
    // the callback runs inside a host session.
    let bridge = unsafe { ContextBridge::new(vtable, handle) };
    match bridge.get("greeting") {
        Some(value) => log(vtable, format!("context get greeting: {value}")),
        None => log(vtable, "context get greeting: <missing>".to_string()),
    }
}

/// The first-registered disposer: records its run order.
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call.
unsafe extern "C" fn disposer_first(handle: *mut PluginHandle) {
    let order = DISPOSER_RUNS.fetch_add(1, Ordering::SeqCst);
    FIRST_ORDER.store(order as i32, Ordering::SeqCst);
    // SAFETY: the host passes a live handle for the current callback.
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance.
    let vtable = unsafe { &*instance.vtable };
    log(vtable, format!("context disposer first (order {order})"));
}

/// The second-registered disposer: records its run order.
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call.
unsafe extern "C" fn disposer_second(handle: *mut PluginHandle) {
    let order = DISPOSER_RUNS.fetch_add(1, Ordering::SeqCst);
    SECOND_ORDER.store(order as i32, Ordering::SeqCst);
    // SAFETY: the host passes a live handle for the current callback.
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance.
    let vtable = unsafe { &*instance.vtable };
    log(vtable, format!("context disposer second (order {order})"));
}
