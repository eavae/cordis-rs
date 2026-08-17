//! Host-side Context bridge for `.so` plugins.
//!
//! The plugin's apply / event callbacks / disposers run on the host thread
//! inside a *session*: a per-handle association with the fiber's
//! [`Context`]. Every vtable entry resolves the session for the handle the
//! plugin passes, so services and events registered by one fiber never leak
//! into another (multiple fibers may share one plugin handle).
//!
//! Values cross the boundary as JSON strings. The host copies payloads
//! during the call; the plugin copies the `get` result before its next host
//! call. No allocation crosses the ABI, and all calls must happen on the
//! host thread.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char};
use std::rc::Rc;

use cordis_core::{Context, Effect, EventCallback, EventOptions, sync_disposer};
use cordis_sdk::{PluginDisposer, PluginEventCallback, PluginHandle};

/// One host→plugin call frame: the plugin handle plus the fiber context the
/// current call belongs to.
struct Session {
    handle: usize,
    ctx: Context,
    /// Scratch buffer for `host_get` results; valid until the next host call
    /// into the same session.
    scratch: RefCell<Option<CString>>,
}

thread_local! {
    static SESSIONS: RefCell<Vec<Session>> = const { RefCell::new(Vec::new()) };
    /// Plugin handles that are still alive (created, not yet disposed).
    /// Deferred host→plugin callbacks (event listeners, disposers) check this
    /// before invoking plugin code, so a plugin instance disposed while its
    /// fiber is still unloading never causes a call into freed code.
    static LIVE_HANDLES: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

/// Marks `handle` as alive (called by `SoPlugin::create`).
pub fn register_handle(handle: *mut PluginHandle) {
    LIVE_HANDLES.with(|live| {
        live.borrow_mut().insert(handle as usize);
    });
}

/// Marks `handle` as disposed (called by `SoPlugin::drop`).
pub fn unregister_handle(handle: *mut PluginHandle) {
    LIVE_HANDLES.with(|live| {
        live.borrow_mut().remove(&(handle as usize));
    });
}

/// Whether the plugin instance behind `handle` is still alive.
pub fn is_handle_live(handle: *mut PluginHandle) -> bool {
    LIVE_HANDLES.with(|live| live.borrow().contains(&(handle as usize)))
}

/// Runs `f` with a session binding `handle` to `ctx` (host thread only).
///
/// Nested sessions are supported: a plugin event callback fired while
/// another session is active pushes its own frame and pops it on return.
pub fn with_session<R>(handle: *mut PluginHandle, ctx: &Context, f: impl FnOnce() -> R) -> R {
    SESSIONS.with(|sessions| {
        sessions.borrow_mut().push(Session {
            handle: handle as usize,
            ctx: ctx.clone(),
            scratch: RefCell::new(None),
        });
    });
    struct SessionGuard(usize);
    impl Drop for SessionGuard {
        fn drop(&mut self) {
            SESSIONS.with(|sessions| {
                let mut sessions = sessions.borrow_mut();
                if sessions
                    .last()
                    .is_some_and(|session| session.handle == self.0)
                {
                    sessions.pop();
                }
            });
        }
    }
    let _guard = SessionGuard(handle as usize);
    f()
}

/// Clones the context of the innermost session matching `handle`.
fn session_ctx(handle: *mut PluginHandle) -> Option<Context> {
    SESSIONS.with(|sessions| {
        sessions
            .borrow()
            .iter()
            .rev()
            .find(|session| session.handle == handle as usize)
            .map(|session| session.ctx.clone())
    })
}

/// Stores `value` in the session's scratch buffer and returns its pointer.
fn set_scratch(handle: *mut PluginHandle, value: String) -> Option<*const c_char> {
    let cstring = CString::new(value).ok()?;
    SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        let session = sessions
            .iter_mut()
            .rev()
            .find(|session| session.handle == handle as usize)?;
        *session.scratch.borrow_mut() = Some(cstring);
        session
            .scratch
            .borrow()
            .as_ref()
            .map(|cstring| cstring.as_c_str().as_ptr())
    })
}

unsafe fn cstr<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the caller promises a NUL-terminated string valid for the call.
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

/// Serializes a store value to JSON when it is data (serde values, strings,
/// numbers); `None` for non-serializable object services.
fn value_to_json(value: &Rc<dyn Any>) -> Option<String> {
    if let Some(value) = value.downcast_ref::<serde_yaml_ng::Value>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<serde_json::Value>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<String>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<&str>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<bool>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<i64>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<u64>() {
        return serde_json::to_string(value).ok();
    }
    if let Some(value) = value.downcast_ref::<f64>() {
        return serde_json::to_string(value).ok();
    }
    None
}

/// Serializes event arguments to a JSON array; non-serializable arguments
/// become `null`.
fn args_to_json(args: &[Rc<dyn Any>]) -> String {
    let items: Vec<serde_json::Value> = args
        .iter()
        .map(|arg| {
            value_to_json(arg)
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// Parses a JSON payload into a `serde_yaml_ng::Value` (the loader's data
/// representation), falling back to `Null` on invalid input.
fn payload_to_value(payload: &str) -> serde_yaml_ng::Value {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|json| serde_yaml_ng::to_value(json).ok())
        .unwrap_or(serde_yaml_ng::Value::Null)
}

/// vtable `provide`: registers a service from the plugin's apply context.
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call; `name` and
/// `payload` must be NUL-terminated UTF-8 strings valid for the call.
pub unsafe extern "C" fn host_provide(
    handle: *mut PluginHandle,
    name: *const c_char,
    payload: *const c_char,
) -> i32 {
    // SAFETY: the caller promises NUL-terminated strings.
    let (Some(name), Some(payload)) = (unsafe { cstr(name) }, unsafe { cstr(payload) }) else {
        return 1;
    };
    let Some(ctx) = session_ctx(handle) else {
        return 1;
    };
    let value = payload_to_value(payload);
    match ctx.provide_str(name, Rc::new(value)) {
        Ok(_) => 0,
        Err(message) => {
            ctx.logger()
                .error(format!("ctx.provide({name:?}) failed: {message}"));
            1
        }
    }
}

/// vtable `get`: reads a service back as a JSON string (host scratch buffer,
/// valid until the next host call into the same session).
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call; `name` must
/// be a NUL-terminated UTF-8 string valid for the call.
pub unsafe extern "C" fn host_get(handle: *mut PluginHandle, name: *const c_char) -> *const c_char {
    // SAFETY: the caller promises a NUL-terminated string.
    let Some(name) = (unsafe { cstr(name) }) else {
        return std::ptr::null();
    };
    let Some(ctx) = session_ctx(handle) else {
        return std::ptr::null();
    };
    let Some(value) = ctx.get_str(name) else {
        return std::ptr::null();
    };
    let Some(json) = value_to_json(&value) else {
        return std::ptr::null();
    };
    set_scratch(handle, json).unwrap_or(std::ptr::null())
}

/// vtable `on`: registers an event listener bound to the plugin's fiber.
///
/// Returns an opaque host-owned listener handle (null on failure). The
/// listener is removed automatically when the fiber unloads.
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call; `event` must
/// be a NUL-terminated UTF-8 string valid for the call; `callback` must be a
/// valid plugin function pointer for the plugin's lifetime.
pub unsafe extern "C" fn host_on(
    handle: *mut PluginHandle,
    event: *const c_char,
    callback: PluginEventCallback,
) -> *mut std::ffi::c_void {
    // SAFETY: the caller promises a NUL-terminated string.
    let Some(event) = (unsafe { cstr(event) }) else {
        return std::ptr::null_mut();
    };
    let Some(ctx) = session_ctx(handle) else {
        return std::ptr::null_mut();
    };
    let handle_for_callback = handle;
    let ctx_for_callback = ctx.clone();
    let callback: EventCallback = Rc::new(move |args: &[Rc<dyn Any>]| {
        let handle = handle_for_callback;
        let ctx = ctx_for_callback.clone();
        let plugin_callback = callback;
        let args = args.to_vec();
        Box::pin(async move {
            if !is_handle_live(handle) {
                ctx.logger().error(format!(
                    "skipping event callback for disposed plugin {:#x}",
                    handle as usize
                ));
                return Ok(None);
            }
            let args_json = args_to_json(&args);
            let args = CString::new(args_json).unwrap_or_else(|_| CString::new("[]").unwrap());
            with_session(handle, &ctx, || {
                // SAFETY: the plugin promised the callback is valid for its
                // lifetime and the handle is live (checked above).
                unsafe { plugin_callback(handle, args.as_ptr()) };
            });
            Ok(None)
        })
    });
    match ctx.on(event, callback, EventOptions::default()) {
        Ok(listener) => Rc::as_ptr(&listener).cast_mut().cast::<std::ffi::c_void>(),
        Err(error) => {
            ctx.logger()
                .error(format!("ctx.on({event:?}) failed: {error}"));
            std::ptr::null_mut()
        }
    }
}

/// vtable `emit`: emits an event with a JSON payload (an array of
/// arguments) parsed on the host side.
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call; `event` and
/// `payload` must be NUL-terminated UTF-8 strings valid for the call.
pub unsafe extern "C" fn host_emit(
    handle: *mut PluginHandle,
    event: *const c_char,
    payload: *const c_char,
) {
    // SAFETY: the caller promises NUL-terminated strings.
    let (Some(event), Some(payload)) = (unsafe { cstr(event) }, unsafe { cstr(payload) }) else {
        return;
    };
    let Some(ctx) = session_ctx(handle) else {
        return;
    };
    let value = payload_to_value(payload);
    let args: Vec<Rc<dyn Any>> = match value {
        serde_yaml_ng::Value::Sequence(items) => items
            .into_iter()
            .map(|item| Rc::new(item) as Rc<dyn Any>)
            .collect(),
        other => vec![Rc::new(other) as Rc<dyn Any>],
    };
    ctx.emit(event, &args);
}

/// vtable `effect_disposer`: registers a fiber-bound disposer that runs when
/// the plugin's fiber unloads (reverse registration order), with a session
/// pushed for the plugin.
///
/// # Safety
///
/// `handle` must be the plugin handle of the current host call; `disposer`
/// must be a valid plugin function pointer for the plugin's lifetime.
pub unsafe extern "C" fn host_effect_disposer(handle: *mut PluginHandle, disposer: PluginDisposer) {
    let Some(ctx) = session_ctx(handle) else {
        return;
    };
    let ctx_for_dispose = ctx.clone();
    let disposer_fn = disposer;
    let disposer_outer = sync_disposer(move || {
        if !is_handle_live(handle) {
            ctx_for_dispose.logger().error(format!(
                "skipping disposer for disposed plugin {:#x}",
                handle as usize
            ));
            return;
        }
        with_session(handle, &ctx_for_dispose, || {
            // SAFETY: the plugin promised the disposer is valid for its
            // lifetime and the handle is live (checked above).
            unsafe { disposer_fn(handle) };
        });
    });
    if let Err(error) = ctx.effect(
        || Effect::Disposer(disposer_outer),
        "cordis.so.effect_disposer",
    ) {
        ctx.logger()
            .error(format!("effect_disposer failed: {error}"));
    }
}
