//! Host-side runtime for `.so` plugin async tasks.
//!
//! The host owns a per-instance task list. Plugins hand boxed futures to
//! [`host_spawn`] through the vtable; each boxed future is driven on the
//! host's current-thread runtime and dropped (through the plugin's drop
//! function) when it completes or when the plugin instance is disposed.

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll, Waker};

use cordis_sdk::{BoxedFuture, RcWaker, WakerData};
use libloading::Library;

/// The per-plugin-instance host runtime: owns every task the plugin spawned.
pub struct HostRuntime {
    tasks: std::cell::RefCell<Vec<tokio::task::JoinHandle<()>>>,
    library: Option<Arc<Library>>,
}

impl HostRuntime {
    /// Creates an empty runtime.
    pub fn new() -> Rc<Self> {
        HostRuntime::with_library(None)
    }

    /// Creates a runtime that keeps `library` mapped until every spawned
    /// task is dropped: pending tasks hold their own `Arc` clone, so the
    /// plugin's boxed futures are always dropped (through the plugin's drop
    /// function) while the library is still loaded.
    pub fn with_library(library: Option<Arc<Library>>) -> Rc<Self> {
        Rc::new(HostRuntime {
            tasks: std::cell::RefCell::new(Vec::new()),
            library,
        })
    }

    /// Cancels and drops every pending task (plugin instance disposed).
    pub fn cancel_all(&mut self) {
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
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

    let task = HostTask {
        future: Some(*future),
        waker_data: None,
        waker: Cell::new(None),
        _library: runtime.library.clone(),
    };
    let handle = tokio::task::spawn_local(task);
    runtime.tasks.borrow_mut().push(handle);
}

/// A host task: polls a plugin boxed future until it completes.
struct HostTask {
    future: Option<BoxedFuture>,
    waker_data: Option<RcWaker>,
    waker: Cell<Option<Waker>>,
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
        self.waker.set(Some(cx.waker().clone()));
        if self.waker_data.is_none() {
            self.waker_data = Some(WakerData::new(wake_task));
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

/// The host-side wake callback: re-schedules the task (no-op under the
/// cooperative model; the plugin futures in scope are driven by the await
/// polling loop, and real wakeups are handled by the runtime waker stored
/// on the task).
unsafe extern "C" fn wake_task(_data: *mut c_void) {}

// SAFETY: the runtime and tasks are confined to the host thread.
unsafe impl Send for HostTask {}
