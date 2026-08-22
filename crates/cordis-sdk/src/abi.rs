//! The `.so` plugin ABI protocol.
//!
//! Hand-written `extern "C"` entry points; cross-boundary objects are opaque
//! handles allocated by the plugin side, and allocation never crosses the
//! boundary. The async bridge lets plugins hand boxed
//! futures to the host runtime; the host polls and drops them via function
//! pointers, so no future object or allocator crosses the boundary.

use std::ffi::{CString, c_char};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};

use tokio::sync::oneshot;

/// The ABI version implemented by this SDK.
///
/// v3 adds the Context bridge (`provide`/`get`/`on`/`emit`/
/// `effect_disposer`) to the host vtable.
pub const PLUGIN_API_VERSION: u32 = 3;

/// Polls a plugin-owned boxed future.
pub type BoxedPoll = unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void) -> u8;

/// Drops a plugin-owned boxed future.
pub type BoxedDrop = unsafe extern "C" fn(*mut std::ffi::c_void);

/// Spawns a plugin-owned boxed future on the host runtime.
pub type HostSpawn = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);

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

/// A future owned by the plugin, polled by the host.
///
/// `data` is an opaque pointer allocated on the plugin side; `poll` and
/// `drop` are plugin functions the host calls back into.
#[repr(C)]
pub struct BoxedFuture {
    /// Plugin-side allocation (the boxed future + completion state).
    pub data: *mut std::ffi::c_void,
    /// Polls the future; returns 1 on `Ready`, 0 on `Pending`.
    pub poll: BoxedPoll,
    /// Drops the plugin-side allocation.
    pub drop: BoxedDrop,
}

/// Host callbacks the plugin can use.
#[repr(C)]
pub struct HostVtable {
    /// Logs a message through the host logger.
    pub log: extern "C" fn(message: *const c_char),
    /// Spawns a boxed plugin future on the host runtime.
    pub spawn: HostSpawn,
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

/// The waker shared between the host and a spawned plugin future.
///
/// The host allocates one per spawned task and hands a clone (an owned raw
/// pointer) to the plugin; both sides refcount it. The plugin side converts
/// the raw pointer into a [`Waker`] with [`waker_from_raw`].
#[repr(C)]
pub struct WakerData {
    refs: AtomicUsize,
    /// Opaque host context handed back to `wake`; the host keeps it valid
    /// for as long as any waker reference is alive (e.g. a per-task slot
    /// holding the tokio waker that re-schedules the task).
    data: *mut std::ffi::c_void,
    wake: unsafe extern "C" fn(*mut std::ffi::c_void),
}

impl WakerData {
    /// Creates a waker data cell owned by the caller; `data` is passed back
    /// to `wake` and must stay valid for the cell's lifetime.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        data: *mut std::ffi::c_void,
        wake: unsafe extern "C" fn(*mut std::ffi::c_void),
    ) -> RcWaker {
        RcWaker(Box::into_raw(Box::new(Self {
            refs: AtomicUsize::new(1),
            data,
            wake,
        })))
    }
}

/// An owning raw pointer to [`WakerData`] (one reference).
pub struct RcWaker(*mut WakerData);

// SAFETY: wakers may be moved between threads by futures that require Send;
// the refcount is atomic and wake is a no-op under the cooperative model.
unsafe impl Send for RcWaker {}
unsafe impl Sync for RcWaker {}

impl RcWaker {
    /// The raw pointer for FFI hand-off (ownership transfers to the caller).
    pub fn leak(self) -> *mut WakerData {
        let ptr = self.0;
        std::mem::forget(self);
        ptr
    }

    /// A borrowed raw pointer (the caller keeps ownership).
    pub fn as_ptr(&self) -> *mut WakerData {
        self.0
    }
}

impl Clone for RcWaker {
    fn clone(&self) -> Self {
        // SAFETY: `self.0` is a valid WakerData with at least one reference.
        unsafe {
            (*self.0).refs.fetch_add(1, Ordering::AcqRel);
        }
        Self(self.0)
    }
}

impl Drop for RcWaker {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid WakerData with at least one reference.
        unsafe {
            if (*self.0).refs.fetch_sub(1, Ordering::AcqRel) == 1 {
                drop(Box::from_raw(self.0));
            }
        }
    }
}

unsafe fn waker_clone(ptr: *const ()) -> RawWaker {
    // SAFETY: the pointer came from `waker_from_raw` / the host.
    let data = unsafe { &*(ptr as *const WakerData) };
    data.refs.fetch_add(1, Ordering::AcqRel);
    RawWaker::new(ptr.cast(), &WAKER_VTABLE)
}

unsafe fn waker_wake(ptr: *const ()) {
    // SAFETY: the pointer is a valid WakerData.
    let data = unsafe { &*(ptr as *const WakerData) };
    // SAFETY: the wake callback was provided by the host.
    unsafe { (data.wake)(data.data) };
}

unsafe fn waker_wake_by_ref(ptr: *const ()) {
    // SAFETY: the pointer is a valid WakerData.
    let data = unsafe { &*(ptr as *const WakerData) };
    // SAFETY: the wake callback was provided by the host.
    unsafe { (data.wake)(data.data) };
}

unsafe fn waker_drop(ptr: *const ()) {
    // SAFETY: the pointer was produced by `waker_clone` / `waker_from_raw`.
    let data = unsafe { &*(ptr as *const WakerData) };
    if data.refs.fetch_sub(1, Ordering::AcqRel) == 1 {
        // SAFETY: the last reference frees the allocation.
        drop(unsafe { Box::from_raw(ptr as *mut WakerData) });
    }
}

/// The raw waker vtable shared by host and plugin.
pub static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

/// Converts a raw waker pointer (from the host) into a [`Waker`].
///
/// # Safety
///
/// `ptr` must come from the host and be valid for the returned waker's
/// lifetime; the waker data is refcounted, so clones keep it alive.
pub unsafe fn waker_from_raw(ptr: *const std::ffi::c_void) -> Waker {
    // SAFETY: the pointer was produced by the host and matches WAKER_VTABLE.
    let data = unsafe { &*(ptr as *const WakerData) };
    data.refs.fetch_add(1, Ordering::AcqRel);
    unsafe { Waker::from_raw(RawWaker::new(ptr.cast(), &WAKER_VTABLE)) }
}

/// Shared completion state of a spawned task (plugin side).
struct SpawnState<T> {
    result: Option<oneshot::Sender<T>>,
}

/// Wraps a plugin future so the host can poll it through [`BoxedFuture`].
struct BoxedPluginFuture<F, T> {
    future: F,
    state: SpawnState<T>,
}

unsafe extern "C" fn boxed_poll<F, T>(
    data: *mut std::ffi::c_void,
    waker: *const std::ffi::c_void,
) -> u8
where
    F: Future<Output = T>,
{
    // SAFETY: `data` is a Box<BoxedPluginFuture<F, T>> allocated by `spawn`.
    let this = unsafe { &mut *(data as *mut BoxedPluginFuture<F, T>) };
    // SAFETY: the host passes a valid raw waker pointer.
    let waker = unsafe { waker_from_raw(waker) };
    let mut cx = TaskContext::from_waker(&waker);
    // SAFETY: the future is pinned in place inside the box.
    match unsafe { Pin::new_unchecked(&mut this.future).poll(&mut cx) } {
        Poll::Ready(value) => {
            if let Some(sender) = this.state.result.take() {
                let _ = sender.send(value);
            }
            1
        }
        Poll::Pending => 0,
    }
}

unsafe extern "C" fn boxed_drop<F, T>(data: *mut std::ffi::c_void)
where
    F: Future<Output = T>,
{
    // SAFETY: `data` is a Box<BoxedPluginFuture<F, T>> allocated by `spawn`.
    drop(unsafe { Box::from_raw(data as *mut BoxedPluginFuture<F, T>) });
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
/// # Safety
///
/// `vtable` must be the host vtable the plugin received from `plugin_create`.
pub unsafe fn spawn<F>(vtable: &HostVtable, future: F) -> Spawned<F::Output>
where
    F: Future + 'static,
{
    let (sender, receiver) = oneshot::channel();
    let boxed = Box::new(BoxedPluginFuture {
        future,
        state: SpawnState {
            result: Some(sender),
        },
    });
    let data: *mut std::ffi::c_void = Box::into_raw(boxed).cast();
    let wrapped = BoxedFuture {
        data,
        poll: boxed_poll::<F, F::Output>,
        drop: boxed_drop::<F, F::Output>,
    };
    // SAFETY: the caller guarantees the vtable; `wrapped` is transferred to
    // the host (dropped by the host through `BoxedFuture.drop`).
    unsafe { (vtable.spawn)(vtable.data, Box::into_raw(Box::new(wrapped)).cast()) };
    Spawned {
        receiver: Some(receiver),
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
