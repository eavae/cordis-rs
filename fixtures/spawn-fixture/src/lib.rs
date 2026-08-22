//! Fixture exercising the host async bridge through the ABI.

#![allow(missing_docs)]

use std::future::poll_fn;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::task::Poll;
use std::time::Duration;

use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle, spawn};

/// Number of times `plugin_create` has been called.
static CREATE_COUNT: AtomicU32 = AtomicU32::new(0);
/// Number of spawned tasks whose boxed future was dropped before completing.
static CANCELLED_DROPS: AtomicU32 = AtomicU32::new(0);
/// Number of completed spawned tasks.
static COMPLETED: AtomicU32 = AtomicU32::new(0);
/// Number of polls of the pending-then-wake fixture's future.
static POLLS: AtomicU32 = AtomicU32::new(0);

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

/// Spawns a task that itself spawns a nested host task and awaits its result
/// through `Spawned`: the SDK completion handshake (oneshot + waker
/// registration) delivers the value across the host runtime.
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_spawn_and_await(handle: *mut PluginHandle) {
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the vtable is the one the host provided.
    let vtable: &'static HostVtable = unsafe { std::mem::transmute(vtable) };
    // SAFETY: the vtable is the one the host provided.
    drop(unsafe {
        spawn(vtable, async move {
            let inner = spawn(vtable, async move { 21u32 });
            let value = inner.await.unwrap_or(0) * 2;
            let message =
                std::ffi::CString::new(format!("spawned and awaited result: {value}")).unwrap();
            (vtable.log)(message.as_ptr());
            COMPLETED.fetch_add(1, Ordering::SeqCst);
            value
        })
    });
}

/// Spawns a task that returns `Pending` once, then is woken from a std
/// thread: the cross-thread wake re-schedules the host task through
/// async-ffi's adapted host waker and the future completes on the next poll.
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_pending_then_wake(handle: *mut PluginHandle) {
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the vtable is the one the host provided.
    let vtable: &'static HostVtable = unsafe { std::mem::transmute(vtable) };
    // SAFETY: the vtable is the one the host provided.
    drop(unsafe {
        spawn(vtable, async move {
            let started = Arc::new(AtomicBool::new(false));
            poll_fn(move |cx| {
                POLLS.fetch_add(1, Ordering::SeqCst);
                if started.swap(true, Ordering::SeqCst) {
                    return Poll::Ready(());
                }
                let waker = cx.waker().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    waker.wake();
                });
                Poll::Pending
            })
            .await;
            let message = std::ffi::CString::new("pending task woke and completed").unwrap();
            (vtable.log)(message.as_ptr());
            COMPLETED.fetch_add(1, Ordering::SeqCst);
        })
    });
}

/// Spawns a task whose waker is handed to a std thread that wakes it after
/// `delay_ms`; the host cancels the task before the wake fires (drop the
/// plugin), and the late wake must not touch freed state.
///
/// # Safety
///
/// `handle` must come from `plugin_create` and stay alive for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_pending_wake_later(handle: *mut PluginHandle, delay_ms: u64) {
    let instance = unsafe { instance(handle) };
    // SAFETY: the host vtable outlives the plugin instance (host contract).
    let vtable = unsafe { &*instance.vtable };
    // SAFETY: the vtable is the one the host provided.
    let vtable: &'static HostVtable = unsafe { std::mem::transmute(vtable) };
    // SAFETY: the vtable is the one the host provided.
    drop(unsafe {
        spawn(vtable, async move {
            let guard = CancelledGuard;
            let started = Arc::new(AtomicBool::new(false));
            poll_fn(move |cx| {
                if started.swap(true, Ordering::SeqCst) {
                    return Poll::Ready(());
                }
                let waker = cx.waker().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(delay_ms));
                    waker.wake();
                });
                Poll::Pending
            })
            .await;
            COMPLETED.fetch_add(1, Ordering::SeqCst);
            let _ = guard;
        })
    });
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

/// The number of polls of the pending-then-wake fixture future.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_polls() -> u32 {
    POLLS.load(Ordering::SeqCst)
}

/// Resets the process-wide counters (the host keeps the library loaded
/// across tests, so counters accumulate otherwise).
#[unsafe(no_mangle)]
pub extern "C" fn plugin_reset_counters() {
    CREATE_COUNT.store(0, Ordering::SeqCst);
    CANCELLED_DROPS.store(0, Ordering::SeqCst);
    COMPLETED.store(0, Ordering::SeqCst);
    POLLS.store(0, Ordering::SeqCst);
}

/// Counts drops of a never-completing future (cancellation detection).
struct CancelledGuard;

impl Drop for CancelledGuard {
    fn drop(&mut self) {
        CANCELLED_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
