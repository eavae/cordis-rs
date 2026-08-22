//! Host-side runtime for `.so` plugin async tasks.
//!
//! The host owns a per-instance task list. Plugins hand [`FfiFuture`]s to
//! [`host_spawn`] through the vtable; each future is driven as a tokio task
//! and dropped (through async-ffi's plugin drop function) when it completes
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
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use async_ffi::{FfiFuture, FutureExt};
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
    /// plugin's `FfiFuture`s are always dropped (through async-ffi's plugin
    /// drop function) while the library is still loaded.
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
/// `data` must be the runtime pointer baked into the vtable; `future` is an
/// `FfiFuture` transferred from the plugin and dropped exactly once (by the
/// task that owns it, through async-ffi's plugin drop function).
pub unsafe extern "C" fn host_spawn(data: *mut c_void, future: FfiFuture<()>) {
    // SAFETY: the vtable data pointer was created by `HostRuntime::new` and
    // the caller keeps the runtime alive.
    let runtime = unsafe { &*(data as *const HostRuntime) };
    let task = HostTask {
        future: Some(future),
        _library: runtime.library.clone(),
    };
    let handle = tokio::task::spawn(task);
    runtime.tasks.lock().push(handle);
}

/// Sleeps on the host runtime (vtable `sleep` entry).
///
/// Must be called from a runtime context: the host timer is created here
/// (`tokio::time::sleep`), and the plugin's caller is inside the runtime by
/// contract (host callback or spawned async code).
pub extern "C" fn host_sleep(_data: *mut c_void, millis: u64) -> FfiFuture<()> {
    tokio::time::sleep(Duration::from_millis(millis)).into_ffi()
}

/// Runs a plugin blocking callback on the host's blocking pool (vtable
/// `spawn_blocking` entry).
///
/// # Safety
///
/// `data` must be the runtime pointer baked into the vtable; `work` is
/// called exactly once on a blocking-pool thread with `arg`; ownership of
/// `arg` transfers to `work`. The library `Arc` is captured by the task so
/// the plugin image stays mapped until the callback returns.
pub unsafe extern "C" fn host_spawn_blocking(
    data: *mut c_void,
    work: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
) {
    // SAFETY: the vtable data pointer was created by `HostRuntime::new` and
    // the caller keeps the runtime alive.
    let runtime = unsafe { &*(data as *const HostRuntime) };
    let library = runtime.library.clone();
    let call = BlockingCall { work, arg };
    tokio::task::spawn_blocking(move || run_blocking_call(library, call));
}

/// A `Send` carrier for the blocking callback: the raw pointers are only
/// dereferenced inside the blocking task that owns this value, so moving
/// them to the blocking pool is sound.
struct BlockingCall {
    work: unsafe extern "C" fn(*mut c_void),
    arg: *mut c_void,
}

// SAFETY: `work` is plugin code kept mapped by the task's library Arc and
// `arg` is owned by the callback; both are used exactly once inside the
// blocking task.
unsafe impl Send for BlockingCall {}

/// Runs a blocking callback, keeping the plugin library mapped for the
/// duration of the call.
fn run_blocking_call(library: Option<Arc<Library>>, call: BlockingCall) {
    let _library = library;
    let BlockingCall { work, arg } = call;
    // SAFETY: `work` is plugin code and stays mapped through `_library`;
    // `arg` is owned by the callback.
    unsafe { work(arg) };
}

/// A host task: polls a plugin [`FfiFuture`] until it completes.
///
/// The `FfiFuture` is `Send` (async-ffi enforces it at construction), so the
/// task may be polled on any tokio worker thread; the waker adaptation and
/// refcounting live inside async-ffi, and the plugin future is dropped in
/// field order below while `_library` still keeps the plugin mapped.
struct HostTask {
    future: Option<FfiFuture<()>>,
    /// Keeps the plugin library mapped until this task is dropped: the
    /// `FfiFuture` field drops first (declaration order), so the plugin's
    /// drop function runs while the library is still loaded.
    _library: Option<Arc<Library>>,
}

impl std::future::Future for HostTask {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let Some(future) = self.future.as_mut() else {
            return Poll::Ready(());
        };
        // `FfiFuture` is `Unpin`; async-ffi adapts the host's real tokio
        // waker into the FFI context for the duration of this poll.
        Pin::new(future).poll(cx)
    }
}
