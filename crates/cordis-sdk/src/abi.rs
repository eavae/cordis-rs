//! The `.so` plugin ABI protocol.
//!
//! Hand-written `extern "C"` entry points; cross-boundary objects are opaque
//! handles allocated by the plugin side, and allocation never crosses the
//! boundary. The async bridge lets plugins hand boxed
//! futures to the host runtime; the host polls and drops them via function
//! pointers (`async_ffi::FfiFuture`), so no future object or allocator
//! crosses the boundary. The waker the host hands into each poll is adapted
//! by `async-ffi` into an FFI-safe borrowed context; ownership and
//! refcounting stay inside the `std::task::Waker` on each side.

use std::ffi::{CString, c_char};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use async_ffi::{FfiFuture, FutureExt};
use tokio::sync::oneshot;

/// The ABI version implemented by this SDK.
///
/// v3 adds the Context bridge (`provide`/`get`/`on`/`emit`/
/// `effect_disposer`) to the host vtable.
/// v4 moves the async bridge to `async-ffi`: `spawn` hands the host an
/// `FfiFuture<()>` instead of a hand-rolled boxed future with a shared
/// refcounted waker cell.
/// v5 adds two host async services to the vtable: `sleep` (host timer) and
/// `spawn_blocking` (host blocking pool), both `FfiFuture`-based.
pub const PLUGIN_API_VERSION: u32 = 5;

/// Spawns a plugin-owned future on the host runtime.
///
/// `future` is an `FfiFuture<()>` produced by the SDK's [`spawn`]; the host
/// polls it as a normal tokio task and drops it (through async-ffi's plugin
/// drop function) on completion or cancellation.
pub type HostSpawn = unsafe extern "C" fn(*mut std::ffi::c_void, FfiFuture<()>);

/// Sleeps on the host runtime.
///
/// Returns an `FfiFuture<()>` built host-side from `tokio::time::sleep`;
/// the plugin awaits it like any future and dropping it cancels the timer.
/// Must be called from a runtime context (host callback or spawned async
/// code), like [`HostSpawn`].
pub type HostSleep = extern "C" fn(*mut std::ffi::c_void, u64) -> FfiFuture<()>;

/// Runs a blocking callback on the host's blocking pool.
///
/// `work` is called exactly once on a blocking-pool thread with `arg`;
/// ownership of `arg` transfers to `work`, which must free it (or leak).
/// The plugin library stays mapped until the callback returns. Must be
/// called from a runtime context, like [`HostSpawn`].
pub type HostSpawnBlocking = unsafe extern "C" fn(
    *mut std::ffi::c_void,
    unsafe extern "C" fn(*mut std::ffi::c_void),
    *mut std::ffi::c_void,
);

/// Validates a config payload (JSON string); 0 = valid, non-zero = invalid.
pub type ValidateConfig = unsafe extern "C" fn(*const c_char) -> i32;

/// Applies a config payload (JSON string); 0 = ok, non-zero = failed.
pub type ApplyConfig = unsafe extern "C" fn(*mut PluginHandle, *const c_char) -> i32;

/// A plugin-side event listener invoked by the host.
///
/// `handle` identifies the plugin instance; `args` is a NUL-terminated JSON
/// array of the event arguments, valid only for the duration of the call.
/// The plugin should copy anything it keeps.
pub type PluginEventCallback = unsafe extern "C" fn(*mut PluginHandle, *const c_char);

/// A plugin-side disposer invoked when the fiber unloads.
pub type PluginDisposer = unsafe extern "C" fn(*mut PluginHandle);

/// Provides a service: `name` and `payload` are NUL-terminated strings; the
/// payload is a JSON value the host copies during the call. Returns 0 on
/// success, non-zero on failure (missing session, duplicate registration).
pub type HostProvide = unsafe extern "C" fn(*mut PluginHandle, *const c_char, *const c_char) -> i32;

/// Reads a service back as a JSON string.
///
/// The returned pointer is host-owned and valid only until the next host
/// call into the same plugin session; the plugin must copy it immediately.
/// Returns null when the service is missing or not JSON-serializable.
pub type HostGet = unsafe extern "C" fn(*mut PluginHandle, *const c_char) -> *const c_char;

/// Registers an event listener; returns an opaque host-owned listener handle
/// (null on failure). The listener is an effect of the plugin's fiber and is
/// removed automatically when the fiber unloads.
pub type HostOn = unsafe extern "C" fn(
    *mut PluginHandle,
    *const c_char,
    PluginEventCallback,
) -> *mut std::ffi::c_void;

/// Emits an event with a JSON payload (a JSON array of arguments) the host
/// copies during the call.
pub type HostEmit = unsafe extern "C" fn(*mut PluginHandle, *const c_char, *const c_char);

/// Registers a disposer on the plugin's current fiber; it runs when the
/// fiber unloads, in reverse registration order.
pub type HostEffectDisposer = unsafe extern "C" fn(*mut PluginHandle, PluginDisposer);

/// Host callbacks the plugin can use.
#[repr(C)]
pub struct HostVtable {
    /// Logs a message through the host logger.
    pub log: extern "C" fn(message: *const c_char),
    /// Spawns a plugin future on the host runtime (see [`HostSpawn`]).
    pub spawn: HostSpawn,
    /// Sleeps on the host runtime (see [`HostSleep`]).
    pub sleep: HostSleep,
    /// Runs a blocking callback on the host's blocking pool (see
    /// [`HostSpawnBlocking`]).
    pub spawn_blocking: HostSpawnBlocking,
    /// Provides a service from plugin apply.
    pub provide: HostProvide,
    /// Reads a service back into the plugin.
    pub get: HostGet,
    /// Registers an event listener.
    pub on: HostOn,
    /// Emits an event.
    pub emit: HostEmit,
    /// Registers a fiber-bound disposer.
    pub effect_disposer: HostEffectDisposer,
    /// Host-side runtime handle passed back into `spawn`.
    pub data: *mut std::ffi::c_void,
    /// The host ABI version (validated by the plugin).
    pub host_version: u32,
}

// SAFETY: the vtable is only used while the owning plugin instance is alive;
// the raw data pointer stays valid for the plugin's lifetime.
unsafe impl Send for HostVtable {}
unsafe impl Sync for HostVtable {}

/// An opaque plugin handle owned by the plugin.
#[repr(C)]
pub struct PluginHandle {
    _private: [u8; 0],
}

/// Returns the plugin ABI version the host must match.
#[cfg(feature = "abi-exports")]
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
#[cfg(feature = "abi-exports")]
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

/// Tears down a plugin instance (no-op in the base protocol).
///
/// # Safety
///
/// `handle` must come from a matching [`plugin_create`] call.
#[cfg(feature = "abi-exports")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(_handle: *mut PluginHandle) {}

/// Returns the plugin metadata as a NUL-terminated JSON string.
///
/// The returned pointer is owned by the plugin and stays valid for the
/// library's lifetime (a `&'static str` on the plugin side).
#[cfg(feature = "abi-exports")]
#[unsafe(no_mangle)]
pub extern "C" fn plugin_meta() -> *const c_char {
    c"{\"name\":\"cordis-sdk\",\"version\":\"0.1.0\",\"inject\":[],\"provide\":[]}".as_ptr()
}

/// Validates a config payload (JSON string); 0 = valid, non-zero = invalid.
///
/// # Safety
///
/// `config` must be a NUL-terminated UTF-8 string.
#[cfg(feature = "abi-exports")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_validate_config(_config: *const c_char) -> i32 {
    0
}

/// Applies a config payload (JSON string); 0 = ok, non-zero = failed.
///
/// # Safety
///
/// `handle` must come from `plugin_create`; `config` must be NUL-terminated.
#[cfg(feature = "abi-exports")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_apply(_handle: *mut PluginHandle, _config: *const c_char) -> i32 {
    0
}

/// A future that resolves to the result of a task spawned on the host.
///
/// The completion is delivered through a oneshot channel and the poll
/// registers the caller's waker, so the awaiting side resumes as soon as the
/// host task completes (on any runtime thread).
pub struct Spawned<T> {
    receiver: Option<oneshot::Receiver<T>>,
}

impl<T> Future for Spawned<T> {
    type Output = Result<T, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let Some(receiver) = self.receiver.as_mut() else {
            return Poll::Ready(Err("task has already been awaited".to_string()));
        };
        match Pin::new(receiver).poll(cx) {
            Poll::Ready(Ok(value)) => {
                self.receiver.take();
                Poll::Ready(Ok(value))
            }
            Poll::Ready(Err(_)) => {
                self.receiver.take();
                Poll::Ready(Err("task was cancelled".to_string()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Spawns a future on the host runtime.
///
/// Wraps `future` as an [`FfiFuture`] (one plugin-side allocation) and
/// hands it to the host through the vtable. The completion value is
/// delivered back through a oneshot channel: `Spawned` registers the
/// caller's waker while pending, so the awaiting side resumes as soon as
/// the host task completes (on any runtime thread).
///
/// # Safety
///
/// `vtable` must be the host vtable the plugin received from `plugin_create`.
pub unsafe fn spawn<F>(vtable: &HostVtable, future: F) -> Spawned<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send,
{
    let (sender, receiver) = oneshot::channel();
    // The oneshot sender moves with the future: dropping the task (cancel)
    // drops the sender and wakes the receiver with an error.
    let ffi: FfiFuture<()> = async move {
        let value = future.await;
        let _ = sender.send(value);
    }
    .into_ffi();
    // SAFETY: the caller guarantees the vtable; `ffi` is transferred to the
    // host (polled and dropped by the host through async-ffi's function
    // pointers).
    unsafe { (vtable.spawn)(vtable.data, ffi) };
    Spawned {
        receiver: Some(receiver),
    }
}

/// Sleeps on the host runtime.
///
/// The returned future is host-built (`tokio::time::sleep` on the host's
/// runtime); awaiting it suspends until `duration` elapses and dropping it
/// cancels the timer. Must be *called* from a runtime context (host
/// callback or spawned async code), like [`spawn`].
pub fn sleep(vtable: &HostVtable, duration: std::time::Duration) -> FfiFuture<()> {
    let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    (vtable.sleep)(vtable.data, millis)
}

/// Runs a blocking closure on the host's blocking pool.
///
/// The closure runs on a host blocking-pool thread and its result is
/// delivered back through a oneshot channel (see [`Spawned`]). Unlike
/// `spawn`, a blocking task cannot be cancelled once started; dropping the
/// returned future only drops the receiver.
///
/// The closure (and its output) must be `Send`: the host may run it on any
/// blocking-pool thread.
pub fn spawn_blocking<F, T>(vtable: &HostVtable, f: F) -> Spawned<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = oneshot::channel();
    let arg = Box::into_raw(Box::new(BlockingArg { f: Some(f), sender }));
    // SAFETY: the caller guarantees the vtable; `arg` is owned by the
    // callback (`run_blocking`), which frees it exactly once.
    unsafe { (vtable.spawn_blocking)(vtable.data, run_blocking::<F, T>, arg.cast()) };
    Spawned {
        receiver: Some(receiver),
    }
}

/// The plugin-side argument handed to the host's blocking-pool callback.
struct BlockingArg<F, T> {
    f: Option<F>,
    sender: oneshot::Sender<T>,
}

/// Runs a blocking closure and forwards its result through the oneshot.
///
/// Owns `arg` (freed exactly once) and runs on a host blocking-pool thread.
unsafe extern "C" fn run_blocking<F, T>(arg: *mut std::ffi::c_void)
where
    F: FnOnce() -> T,
{
    // SAFETY: `arg` is a Box<BlockingArg<F, T>> allocated by `spawn_blocking`
    // and owned by this callback.
    let arg = unsafe { Box::from_raw(arg as *mut BlockingArg<F, T>) };
    let BlockingArg { f, sender } = *arg;
    if let Some(f) = f {
        let _ = sender.send(f());
    }
}

/// Awaits `future`, timing out after `duration` on the host runtime.
///
/// Returns `Err(())` if the timer elapses first; the inner future is then
/// dropped (cancelled). Composes the host `sleep` service with the plugin
/// future — no additional ABI surface.
pub async fn timeout<F>(
    vtable: &HostVtable,
    duration: std::time::Duration,
    future: F,
) -> Result<F::Output, ()>
where
    F: std::future::Future,
{
    let mut timer = Box::pin(sleep(vtable, duration));
    let mut future = Box::pin(future);
    std::future::poll_fn(move |cx| {
        // The inner future wins when both are ready in the same poll.
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(())),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

/// A periodic timer driven by the host runtime.
///
/// Built from the host `sleep` service: each `tick` waits until the next
/// period boundary, so ticks do not drift with the work done between them.
pub struct Interval<'a> {
    vtable: &'a HostVtable,
    period: std::time::Duration,
    deadline: Option<std::time::Instant>,
}

impl<'a> Interval<'a> {
    /// Creates an interval whose first tick fires after `period`.
    pub fn new(vtable: &'a HostVtable, period: std::time::Duration) -> Self {
        Interval {
            vtable,
            period,
            deadline: None,
        }
    }

    /// Waits until the next tick.
    pub async fn tick(&mut self) {
        let now = std::time::Instant::now();
        let deadline = self.deadline.unwrap_or(now + self.period);
        self.deadline = Some(deadline + self.period);
        sleep(self.vtable, deadline.saturating_duration_since(now)).await;
    }
}

/// The plugin-side view of the host's [`Context`](crate::Context) surface:
/// services, events and fiber-bound disposers, all bridged
/// through the host vtable.
///
/// A bridge is only valid while the host is calling into the plugin (apply,
/// an event callback, or a disposer); the host pushes a session for the
/// plugin's handle on the calling thread for the duration of those calls.
///
/// Values cross the boundary as JSON strings: the host copies them during
/// the call, so no allocation crosses the ABI.
pub struct ContextBridge<'a> {
    vtable: &'a HostVtable,
    handle: *mut PluginHandle,
}

impl<'a> ContextBridge<'a> {
    /// Creates a bridge for the current host call.
    ///
    /// # Safety
    ///
    /// `vtable` must be the host vtable this plugin was created with and
    /// `handle` the handle the host passed into the current call.
    pub unsafe fn new(vtable: &'a HostVtable, handle: *mut PluginHandle) -> Self {
        ContextBridge { vtable, handle }
    }

    /// Provides a service (`ctx.provide(name, value)` in the core).
    ///
    /// `payload` is the JSON encoding of the value; the host stores it as a
    /// `serde_yaml_ng::Value` and disposes the registration with the fiber.
    pub fn provide(&self, name: &str, payload: &str) -> Result<(), String> {
        let name = CString::new(name).map_err(|error| error.to_string())?;
        let payload = CString::new(payload).map_err(|error| error.to_string())?;
        // SAFETY: the vtable and handle are valid for the current host call.
        let result = unsafe { (self.vtable.provide)(self.handle, name.as_ptr(), payload.as_ptr()) };
        if result == 0 {
            Ok(())
        } else {
            Err(format!("host rejected ctx.provide({name:?})"))
        }
    }

    /// Reads a service (`ctx.get(name)` in the core).
    ///
    /// The host serializes the value to JSON; non-serializable services
    /// (e.g. Rust object services) come back as `None`.
    pub fn get(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        // SAFETY: the vtable and handle are valid for the current host call;
        // the returned string is copied before the next host call.
        let ptr = unsafe { (self.vtable.get)(self.handle, name.as_ptr()) };
        if ptr.is_null() {
            return None;
        }
        // SAFETY: the host returns a NUL-terminated string.
        Some(
            unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// Registers an event listener (`ctx.on(event, cb)` in the core).
    ///
    /// Returns an opaque host-owned listener handle; the listener is removed
    /// automatically when the plugin's fiber unloads.
    pub fn on(
        &self,
        event: &str,
        callback: PluginEventCallback,
    ) -> Result<*mut std::ffi::c_void, String> {
        let event = CString::new(event).map_err(|error| error.to_string())?;
        // SAFETY: the vtable and handle are valid for the current host call.
        let listener = unsafe { (self.vtable.on)(self.handle, event.as_ptr(), callback) };
        if listener.is_null() {
            Err(format!("host rejected ctx.on({event:?})"))
        } else {
            Ok(listener)
        }
    }

    /// Emits an event (`ctx.emit(event, ...)` in the core).
    ///
    /// `payload` must be a JSON array encoding the event arguments.
    pub fn emit(&self, event: &str, payload: &str) {
        let event = CString::new(event).expect("event has no NUL");
        let payload = CString::new(payload).expect("payload has no NUL");
        // SAFETY: the vtable and handle are valid for the current host call.
        unsafe { (self.vtable.emit)(self.handle, event.as_ptr(), payload.as_ptr()) };
    }

    /// Registers a fiber-bound disposer (`Effect::Disposer` in the core).
    ///
    /// The disposer runs when the plugin's fiber unloads, in reverse
    /// registration order, with a session pushed for `handle`.
    pub fn effect_disposer(&self, disposer: PluginDisposer) {
        // SAFETY: the vtable and handle are valid for the current host call.
        unsafe { (self.vtable.effect_disposer)(self.handle, disposer) };
    }
}
