//! Host-side dynamic loader for `.so` plugins (story card E3).

use std::fmt;
use std::path::{Path, PathBuf};

use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle};
use libloading::{Library, Symbol};

type ApiVersion = unsafe extern "C" fn() -> u32;
type Create = unsafe extern "C" fn(*const HostVtable) -> *mut PluginHandle;
type Dispose = unsafe extern "C" fn(*mut PluginHandle);

/// Errors produced by the dynamic loader.
#[derive(Debug)]
pub enum LoadError {
    /// The library could not be opened.
    Open { path: PathBuf, error: String },
    /// A required symbol is missing.
    MissingSymbol {
        path: PathBuf,
        symbol: &'static str,
        error: String,
    },
    /// The plugin exports an unsupported ABI version.
    VersionMismatch {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Open { path, error } => {
                write!(f, "cannot open plugin {path:?}: {error}")
            }
            LoadError::MissingSymbol {
                path,
                symbol,
                error,
            } => write!(f, "plugin {path:?} is missing symbol {symbol}: {error}"),
            LoadError::VersionMismatch {
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
/// Dropping the handle calls `plugin_dispose` (mirrors the E2 protocol).
pub struct SoPlugin {
    path: PathBuf,
    version: u32,
    // Heap-pinned so the symbol references below stay valid after the struct
    // moves; the library is unloaded when the box is dropped.
    _library: Box<Library>,
    handle: Option<*mut PluginHandle>,
    create: Symbol<'static, Create>,
    dispose: Symbol<'static, Dispose>,
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
    pub unsafe fn load(path: &Path) -> Result<SoPlugin, LoadError> {
        // SAFETY: libloading requires the caller to keep the path valid.
        let library = unsafe { Library::new(path) }.map_err(|error| LoadError::Open {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
        // Heap-pinning lets the symbols borrow the library with a static
        // lifetime; the box is moved into the struct without moving the
        // library itself.
        let library = Box::new(library);
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
        Ok(SoPlugin {
            path: path.to_path_buf(),
            version: found,
            _library: library,
            handle: None,
            create,
            dispose,
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

    /// Calls `plugin_create` with `vtable`; returns the opaque handle.
    ///
    /// # Safety
    ///
    /// `vtable` must be valid and outlive the returned handle.
    pub unsafe fn create(&mut self, vtable: &HostVtable) -> *mut PluginHandle {
        // SAFETY: the create symbol is valid for the library lifetime.
        let handle = unsafe { (self.create)(vtable) };
        self.handle = if handle.is_null() { None } else { Some(handle) };
        handle
    }
}

impl Drop for SoPlugin {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            // SAFETY: the handle came from plugin_create; the symbols are
            // still valid (the library is alive until after this call).
            unsafe { (self.dispose)(handle) };
        }
    }
}

/// Whether a name looks like a native plugin path.
pub fn is_plugin_path(name: &str) -> bool {
    name.ends_with(".so") || name.ends_with(".dylib") || name.ends_with(".dll")
}
