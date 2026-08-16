//! The `.so` plugin ABI protocol (story card E2).
//!
//! Hand-written `extern "C"` entry points; cross-boundary objects are opaque
//! handles allocated by the plugin side, and allocation never crosses the
//! boundary. The async bridge (story card E4) lets plugins hand boxed
//! futures to the host runtime; the host polls and drops them via function
//! pointers, so no future object or allocator crosses the boundary.

use std::ffi::c_char;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};

/// The ABI version implemented by this SDK.
pub const PLUGIN_API_VERSION: u32 = 2;

/// Polls a plugin-owned boxed future.
pub type BoxedPoll = unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void) -> u8;

/// Drops a plugin-owned boxed future.
pub type BoxedDrop = unsafe extern "C" fn(*mut std::ffi::c_void);

/// Spawns a plugin-owned boxed future on the host runtime.
pub type HostSpawn = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void);

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
    /// Spawns a boxed plugin future on the host runtime (story card E4).
    pub spawn: HostSpawn,
    /// Host-side runtime handle passed back into `spawn`.
    pub data: *mut std::ffi::c_void,
    /// The host ABI version (validated by the plugin).
    pub host_version: u32,
}

// SAFETY: the vtable is only used on the host thread; the raw data pointer
// stays valid for the plugin's lifetime.
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

/// Tears down a plugin instance (no-op for the E2 protocol).
///
/// # Safety
///
/// `handle` must come from a matching [`plugin_create`] call.
#[cfg(feature = "abi-exports")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(_handle: *mut PluginHandle) {}

/// The waker shared between the host and a spawned plugin future.
///
/// The host allocates one per spawned task and hands a clone (an owned raw
/// pointer) to the plugin; both sides refcount it. The plugin side converts
/// the raw pointer into a [`Waker`] with [`waker_from_raw`].
#[repr(C)]
pub struct WakerData {
    refs: AtomicUsize,
    wake: unsafe extern "C" fn(*mut std::ffi::c_void),
}

impl WakerData {
    /// Creates a waker data cell owned by the caller.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(wake: unsafe extern "C" fn(*mut std::ffi::c_void)) -> RcWaker {
        RcWaker(Box::into_raw(Box::new(WakerData {
            refs: AtomicUsize::new(1),
            wake,
        })))
    }
}

/// An owning raw pointer to [`WakerData`] (one reference).
pub struct RcWaker(*mut WakerData);

// SAFETY: wakers may be moved between threads by futures that require Send;
// the refcount is atomic and wake is a no-op for E4's cooperative model.
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
        RcWaker(self.0)
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
    unsafe { (data.wake)(ptr as *mut std::ffi::c_void) };
}

unsafe fn waker_wake_by_ref(ptr: *const ()) {
    // SAFETY: the pointer is a valid WakerData.
    let data = unsafe { &*(ptr as *const WakerData) };
    // SAFETY: the wake callback was provided by the host.
    unsafe { (data.wake)(ptr as *mut std::ffi::c_void) };
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
    result: mpsc::Sender<T>,
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
            let _ = this.state.result.send(value);
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
pub struct Spawned<T> {
    receiver: Option<mpsc::Receiver<T>>,
}

impl<T> Future for Spawned<T> {
    type Output = Result<T, String>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        let Some(receiver) = self.receiver.as_ref() else {
            return Poll::Ready(Err("task has already been awaited".to_string()));
        };
        match receiver.try_recv() {
            Ok(value) => {
                self.receiver.take();
                Poll::Ready(Ok(value))
            }
            Err(mpsc::TryRecvError::Empty) => {
                // The host runtime drives the spawned task; yielding lets it
                // make progress on the single-threaded executor.
                Poll::Pending
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.receiver.take();
                Poll::Ready(Err("task was cancelled".to_string()))
            }
        }
    }
}

/// Spawns a future on the host runtime (story card E4).
///
/// # Safety
///
/// `vtable` must be the host vtable the plugin received from `plugin_create`.
pub unsafe fn spawn<F>(vtable: &HostVtable, future: F) -> Spawned<F::Output>
where
    F: Future + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let boxed = Box::new(BoxedPluginFuture {
        future,
        state: SpawnState { result: sender },
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
