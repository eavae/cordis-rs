//! Host-side dynamic loader for `.so` plugins.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cordis_sdk::{
    HostVtable, PLUGIN_API_VERSION, PluginHandle,
    abi::{ApplyConfig, ValidateConfig},
};
use libloading::{Library, Symbol};

use crate::context_bridge;
use crate::host_runtime::{HostRuntime, host_spawn};
use crate::plugin_meta::PluginMeta;

type ApiVersion = unsafe extern "C" fn() -> u32;
type Create = unsafe extern "C" fn(*const HostVtable) -> *mut PluginHandle;
type Dispose = unsafe extern "C" fn(*mut PluginHandle);
type Meta = unsafe extern "C" fn() -> *const std::ffi::c_char;

/// Errors produced by the dynamic loader.
#[derive(Debug)]
pub enum LoadError {
    /// The library could not be opened.
    Open {
        /// The path that could not be opened.
        path: PathBuf,
        /// The underlying loader error.
        error: String,
    },
    /// A required symbol is missing.
    MissingSymbol {
        /// The plugin path.
        path: PathBuf,
        /// The missing symbol name.
        symbol: &'static str,
        /// The underlying loader error.
        error: String,
    },
    /// The plugin exports an unsupported ABI version.
    VersionMismatch {
        /// The plugin path.
        path: PathBuf,
        /// The ABI version exported by the plugin.
        found: u32,
        /// The ABI version required by the host.
        expected: u32,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, error } => {
                write!(f, "cannot open plugin {path:?}: {error}")
            }
            Self::MissingSymbol {
                path,
                symbol,
                error,
            } => write!(f, "plugin {path:?} is missing symbol {symbol}: {error}"),
            Self::VersionMismatch {
                path,
                found,
                expected,
            } => write!(
                f,
                "plugin {path:?} exports ABI version {found}, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for LoadError {}

/// A loaded plugin library.
///
/// Dropping the handle calls `plugin_dispose`.
///
/// On macOS the underlying image may stay mapped after the drop: dyld never
/// unloads images with thread-local storage (see `host_runtime`).
pub struct SoPlugin {
    path: PathBuf,
    version: u32,
    // Heap-pinned (`Arc`) so the symbol references below stay valid after
    // the struct moves; the pending host tasks also hold clones, so the
    // library stays mapped until the last task drops (the plugin's boxed
    // futures are dropped through the plugin's drop function while the
    // library is still loaded). On macOS the image may be retained regardless
    // (dylibs with thread-local storage are never unloaded by dyld).
    _library: Arc<Library>,
    /// Per-instance host runtime: owns every task the plugin spawned;
    /// disposed together with the plugin handle.
    runtime: Arc<HostRuntime>,
    /// The vtable handed to the plugin; kept alive for the plugin's lifetime
    /// (the plugin stores a raw pointer to it).
    vtable: Option<Box<HostVtable>>,
    handle: Option<*mut PluginHandle>,
    create: Symbol<'static, Create>,
    dispose: Symbol<'static, Dispose>,
    meta: Option<Symbol<'static, Meta>>,
    validate: Option<Symbol<'static, ValidateConfig>>,
    apply: Option<Symbol<'static, ApplyConfig>>,
}

// SAFETY: the handle is only touched on the host thread; `Send` is required
// because `Library` is `Send`.
unsafe impl Send for SoPlugin {}

impl SoPlugin {
    /// Loads a plugin library and validates its ABI version.
    ///
    /// # Safety
    ///
    /// The returned plugin must only be used from one thread.
    pub unsafe fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: libloading requires the caller to keep the path valid.
        let library = unsafe { Library::new(path) }.map_err(|error| LoadError::Open {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
        // Heap-pinning (via `Arc`) lets the symbols borrow the library with
        // a static lifetime; the `Arc` is moved into the struct (and cloned
        // into the host runtime) without moving the library itself.
        let library = Arc::new(library);
        let version: Symbol<ApiVersion> =
            unsafe { library.get(b"plugin_api_version") }.map_err(|error| {
                LoadError::MissingSymbol {
                    path: path.to_path_buf(),
                    symbol: "plugin_api_version",
                    error: error.to_string(),
                }
            })?;
        let found = unsafe { version() };
        // `version` goes out of scope here; the library outlives it.
        if found != PLUGIN_API_VERSION {
            return Err(LoadError::VersionMismatch {
                path: path.to_path_buf(),
                found,
                expected: PLUGIN_API_VERSION,
            });
        }
        let create: Symbol<'static, Create> = unsafe {
            std::mem::transmute::<Symbol<Create>, Symbol<'static, Create>>(
                library
                    .get(b"plugin_create")
                    .map_err(|error| LoadError::MissingSymbol {
                        path: path.to_path_buf(),
                        symbol: "plugin_create",
                        error: error.to_string(),
                    })?,
            )
        };
        let dispose: Symbol<'static, Dispose> = unsafe {
            std::mem::transmute::<Symbol<Dispose>, Symbol<'static, Dispose>>(
                library
                    .get(b"plugin_dispose")
                    .map_err(|error| LoadError::MissingSymbol {
                        path: path.to_path_buf(),
                        symbol: "plugin_dispose",
                        error: error.to_string(),
                    })?,
            )
        };
        // Metadata, config validation and apply are optional symbols (older
        // plugins may only export the base entry protocol).
        let meta = unsafe { library.get(b"plugin_meta") }
            .ok()
            .map(|symbol: Symbol<Meta>| {
                // SAFETY: the symbol is moved together with the library into the
                // struct; the library outlives it.
                unsafe { std::mem::transmute::<Symbol<Meta>, Symbol<'static, Meta>>(symbol) }
            });
        let validate = unsafe { library.get(b"plugin_validate_config") }.ok().map(
            |symbol: Symbol<ValidateConfig>| {
                // SAFETY: see above.
                unsafe {
                    std::mem::transmute::<Symbol<ValidateConfig>, Symbol<'static, ValidateConfig>>(
                        symbol,
                    )
                }
            },
        );
        let apply =
            unsafe { library.get(b"plugin_apply") }
                .ok()
                .map(|symbol: Symbol<ApplyConfig>| {
                    // SAFETY: see above.
                    unsafe {
                        std::mem::transmute::<Symbol<ApplyConfig>, Symbol<'static, ApplyConfig>>(
                            symbol,
                        )
                    }
                });
        Ok(Self {
            path: path.to_path_buf(),
            version: found,
            runtime: HostRuntime::with_library(Some(Arc::clone(&library))),
            _library: library,
            vtable: None,
            handle: None,
            create,
            dispose,
            meta,
            validate,
            apply,
        })
    }

    /// The validated ABI version.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The library path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The plugin metadata, when the plugin exports `plugin_meta`.
    pub fn metadata(&self) -> Option<Result<PluginMeta, String>> {
        self.meta.as_ref().map(|meta| {
            // SAFETY: the symbol is valid for the library lifetime.
            let ptr = unsafe { meta() };
            if ptr.is_null() {
                return Err("plugin_meta returned null".to_string());
            }
            // SAFETY: the plugin returns a NUL-terminated string.
            let raw = unsafe { std::ffi::CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned();
            serde_json::from_str(&raw).map_err(|error| format!("invalid plugin metadata: {error}"))
        })
    }

    /// The config validator, when exported by the plugin.
    pub fn validator(&self) -> Option<ValidateConfig> {
        self.validate.as_ref().map(|symbol| **symbol)
    }

    /// The apply entry, when exported by the plugin.
    pub fn apply_fn(&self) -> Option<ApplyConfig> {
        self.apply.as_ref().map(|symbol| **symbol)
    }

    /// The plugin instance handle (the value returned by `plugin_create`).
    pub fn handle_ptr(&self) -> Option<*mut PluginHandle> {
        self.handle
    }

    /// Calls `plugin_create` with a vtable built for this instance; returns
    /// the opaque handle.
    ///
    /// # Safety
    ///
    /// `log` must be callable for the plugin's lifetime.
    pub unsafe fn create(
        &mut self,
        log: extern "C" fn(*const std::ffi::c_char),
    ) -> *mut PluginHandle {
        let vtable = host_vtable(log, &self.runtime);
        self.vtable = Some(Box::new(vtable));
        // SAFETY: the create symbol is valid for the library lifetime; the
        // vtable stays alive because the runtime is owned by this instance.
        let vtable: &HostVtable = self.vtable.as_ref().expect("vtable");
        let handle = unsafe { (self.create)(vtable) };
        self.handle = if handle.is_null() { None } else { Some(handle) };
        if !handle.is_null() {
            // The bridge resolves vtable calls by handle; keep it alive in
            // the registry until this instance is dropped.
            context_bridge::register_handle(handle);
        }
        handle
    }
}

/// Builds a host vtable wired to `runtime`.
///
/// The returned vtable's `data` points to the runtime; the caller must keep
/// `runtime` alive for as long as the vtable (and any plugin created with it)
/// is used.
pub fn host_vtable(
    log: extern "C" fn(*const std::ffi::c_char),
    runtime: &HostRuntime,
) -> HostVtable {
    HostVtable {
        log,
        spawn: host_spawn,
        provide: context_bridge::host_provide,
        get: context_bridge::host_get,
        on: context_bridge::host_on,
        emit: context_bridge::host_emit,
        effect_disposer: context_bridge::host_effect_disposer,
        data: runtime as *const HostRuntime as *mut std::ffi::c_void,
        host_version: PLUGIN_API_VERSION,
    }
}

impl Drop for SoPlugin {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: the handle came from plugin_create; the symbols are
            // still valid (the library is alive until after this call).
            unsafe { (self.dispose)(handle) };
            context_bridge::unregister_handle(handle);
        }
        // Cancel pending spawned futures (their boxed futures are dropped
        // through the plugin's drop function).
        if let Some(runtime) = Arc::get_mut(&mut self.runtime) {
            runtime.cancel_all();
        }
    }
}

/// Whether a name looks like a native plugin path.
pub fn is_plugin_path(name: &str) -> bool {
    name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll")
}
