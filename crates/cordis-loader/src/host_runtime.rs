//! Host-side runtime for `.so` plugin async tasks.
//!
//! The host owns a per-instance task list. Plugins hand boxed futures to
//! [`host_spawn`] through the vtable; each boxed future is driven as a tokio
//! task and dropped (through the plugin's drop function) when it completes
//! or when the plugin instance is disposed.
//!
//! Platform note: on macOS, `dlclose` never unloads an image that contains
//! thread-local storage (a documented dyld limitation), and plugin dylibs
//! embed TLS through their tokio dependency. Such images stay mapped for the
//! process lifetime even after every task is dropped; the `Arc`-based
//! library ownership below matters on Linux/Windows, where `dlclose` really
//! unloads and dropping a plugin with pending tasks would otherwise call
//! into unmapped code.

use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};

use cordis_sdk::{BoxedFuture, RcWaker, WakerData};
use libloading::Library;

/// The per-plugin-instance host runtime: owns every task the plugin spawned.
pub struct HostRuntime {
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    library: Option<Arc<Library>>,
}

impl HostRuntime {
    /// Creates an empty runtime.
    pub fn new() -> Arc<Self> {
        Self::with_library(None)
    }

    /// Creates a runtime that keeps `library` mapped until every spawned
    /// task is dropped: pending tasks hold their own `Arc` clone, so the
    /// plugin's boxed futures are always dropped (through the plugin's drop
    /// function) while the library is still loaded.
    pub fn with_library(library: Option<Arc<Library>>) -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(Vec::new()),
            library,
        })
    }

    /// Cancels and drops every pending task (plugin instance disposed).
    pub fn cancel_all(&mut self) {
        let tasks = std::mem::take(&mut *self.tasks.lock());
        for task in tasks {
            task.abort();
        }
    }
}

/// Spawns a plugin future (vtable `spawn` entry).
///
/// # Safety
///
/// `data` must be the runtime pointer baked into the vtable; `future` must be
/// a `Box<BoxedFuture>` transferred from the plugin and dropped exactly once
/// (here, through [`BoxedFuture::drop`]).
pub unsafe extern "C" fn host_spawn(data: *mut c_void, future: *mut c_void) {
    // SAFETY: the vtable data pointer was created by `HostRuntime::new` and
    // the caller keeps the runtime alive.
    let runtime = unsafe { &*(data as *const HostRuntime) };
    // SAFETY: `future` is a Box<BoxedFuture> allocated by the plugin side.
    let future = unsafe { Box::from_raw(future as *mut BoxedFuture) };

    let wake_slot = Arc::new(WakerSlot {
        waker: Mutex::new(None),
    });
    let task = HostTask {
        future: Some(*future),
        waker_data: None,
        wake_slot,
        _library: runtime.library.clone(),
    };
    let handle = tokio::task::spawn(task);
    runtime.tasks.lock().push(handle);
}

/// The per-task waker slot shared with the plugin's wake callback: `poll`
/// stores the tokio waker, `wake_task` reads it to re-schedule the task from
/// whatever thread the plugin woke it on.
struct WakerSlot {
    waker: Mutex<Option<Waker>>,
}

/// A host task: polls a plugin boxed future until it completes.
struct HostTask {
    future: Option<BoxedFuture>,
    waker_data: Option<RcWaker>,
    wake_slot: Arc<WakerSlot>,
    /// Keeps the plugin library mapped until this task is dropped: the
    /// boxed future is dropped in [`Drop for HostTask`] while the library is
    /// still loaded.
    _library: Option<Arc<Library>>,
}

impl std::future::Future for HostTask {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let Some(future) = self.future.take() else {
            return Poll::Ready(());
        };
        // Hand the runtime waker to the plugin so its future can wake us.
        *self.wake_slot.waker.lock() = Some(cx.waker().clone());
        if self.waker_data.is_none() {
            // SAFETY: the task keeps its `Arc<WakerSlot>` clone alive until
            // the plugin future (and every waker reference it holds) has
            // been dropped, so the raw pointer stays valid for `wake_task`.
            self.waker_data = Some(WakerData::new(
                Arc::as_ptr(&self.wake_slot).cast_mut().cast(),
                wake_task,
            ));
        }
        let waker_ptr = self
            .waker_data
            .as_ref()
            .expect("waker data")
            .as_ptr()
            .cast();
        // SAFETY: `future.data` is the plugin allocation; `raw.as_ptr()`
        // stays valid for the plugin (refcounted) and is freed by its drop.
        let ready = unsafe { (future.poll)(future.data, waker_ptr) };
        if ready == 1 {
            Poll::Ready(())
        } else {
            self.future = Some(future);
            Poll::Pending
        }
    }
}

impl Drop for HostTask {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            // SAFETY: the plugin promised to keep the boxed future valid for
            // the task lifetime; drop it through the plugin's drop fn.
            unsafe { (future.drop)(future.data) };
        }
    }
}

/// The host-side wake callback: re-schedules the task through the tokio
/// waker stored in its [`WakerSlot`], so a wake from the plugin (possibly on
/// another runtime thread) resumes the task instead of relying on a
/// single-threaded polling loop.
unsafe extern "C" fn wake_task(data: *mut c_void) {
    // SAFETY: `data` comes from `WakerData::new` in `HostTask::poll` and
    // points to the task's `WakerSlot`, which stays alive until the task and
    // all of its waker references are dropped.
    let slot = unsafe { &*(data as *const WakerSlot) };
    if let Some(waker) = slot.waker.lock().clone() {
        waker.wake();
    }
}

// SAFETY: the plugin future is opaque to the host (polled through the
// plugin's own function pointers), so the task only moves the `BoxedFuture`
// handle between polls. Plugin futures are assumed `Send` under the stage-2
// contract; per-plugin thread affinity is finalized in stage 3.
unsafe impl Send for HostTask {}
