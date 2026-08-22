//! cordis-cli startup path.

use std::path::Path;
use std::sync::Arc;

use cordis_core::Context;
use cordis_loader::{Loader, SoPlugin, parse_config};

/// CLI options (a subset of the TS `bin.js` surface).
#[derive(Clone, Debug, Default)]
pub struct CliOptions {
    /// Config file (yaml/json); default `cordis.yml`.
    pub config: Option<String>,
    /// Directory to scan for `.so` plugins; default `plugins`.
    pub plugins_dir: Option<String>,
}

/// Scaffolds a new cordis project.
pub fn create_project(dir: &Path, force: bool) -> anyhow::Result<()> {
    if dir.exists() {
        if !force {
            anyhow::bail!(
                "directory {} already exists (use --force to overwrite)",
                dir.display()
            );
        }
    } else {
        std::fs::create_dir_all(dir)?;
    }
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cordis-app")
        .to_string();

    std::fs::create_dir_all(dir.join("plugins/hello/src"))?;

    std::fs::write(
        dir.join("Cargo.toml"),
        r#"[workspace]
members = ["app", "plugins/hello"]
resolver = "3"
"#,
    )?;
    std::fs::create_dir_all(dir.join("app/src"))?;
    std::fs::write(
        dir.join("app/Cargo.toml"),
        format!(
            r#"[package]
name = "{name}-app"
version = "0.1.0"
edition = "2024"

[dependencies]
cordis-cli = {{ path = "{repo}" }}
anyhow = "1"
tokio = {{ version = "1", features = ["rt", "rt-multi-thread", "macros", "time", "sync", "signal"] }}
"#,
            repo = crate_path("cordis-cli")
        ),
    )?;
    std::fs::write(
        dir.join("app/src/main.rs"),
        "fn main() -> anyhow::Result<()> {\n    // The default project watches `./plugins` and reads `./cordis.yml`.\n    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;\n    runtime.block_on(async { cordis_cli::run(&Default::default()).await })\n}\n",
    )?;
    std::fs::write(
        dir.join("cordis.yml"),
        "- id: 'hello'\n  name: cordis-hello\n",
    )?;

    // Example plugin (a `.so` that logs through the host vtable on apply).
    std::fs::write(
        dir.join("plugins/hello/Cargo.toml"),
        format!(
            r#"[package]
name = "cordis-hello"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
cordis-sdk = {{ path = "{repo}", default-features = false }}

[lints.rust]
unsafe_code = "allow"
"#,
            repo = crate_path("cordis-sdk")
        ),
    )?;
    std::fs::write(
        dir.join("plugins/hello/src/lib.rs"),
        r#"//! The example cordis plugin: logs through the host vtable on apply.

use std::sync::atomic::{AtomicU32, Ordering};

use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle};

const META: &std::ffi::CStr =
    c"{\"name\":\"cordis-hello\",\"version\":\"0.1.0\",\"inject\":[],\"provide\":[]}";

struct PluginInstance {
    vtable: *const HostVtable,
    apply_count: AtomicU32,
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
    let instance = Box::new(PluginInstance {
        vtable: host,
        apply_count: AtomicU32::new(0),
    });
    Box::into_raw(instance).cast::<PluginHandle>()
}

/// # Safety
///
/// `handle` must come from a matching create call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(handle: *mut PluginHandle) {
    if handle.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(handle as *mut PluginInstance) });
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_meta() -> *const std::ffi::c_char {
    META.as_ptr()
}

/// # Safety
///
/// `config` must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_validate_config(_config: *const std::ffi::c_char) -> i32 {
    0
}

/// # Safety
///
/// `handle` must come from `plugin_create`; `config` must be NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_apply(handle: *mut PluginHandle, _config: *const std::ffi::c_char) -> i32 {
    // SAFETY: the handle came from plugin_create and is alive.
    let instance = unsafe { &*(handle as *mut PluginInstance) };
    instance.apply_count.fetch_add(1, Ordering::SeqCst);
    // SAFETY: the host vtable outlives the plugin instance.
    let vtable = unsafe { &*instance.vtable };
    let message = std::ffi::CString::new("hello from the example cordis plugin").unwrap();
    (vtable.log)(message.as_ptr());
    0
}
"#,
    )?;
    Ok(())
}

fn crate_path(crate_name: &str) -> String {
    // The CLI crate lives at <workspace>/crates/cordis-cli; the template
    // references sibling crates by their absolute path so the generated
    // project builds against the local SDK no matter where it is scaffolded.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let workspace = Path::new(&manifest)
        .parent()
        .and_then(|path| path.parent())
        .unwrap_or_else(|| Path::new("."));
    let root = workspace.join("crates").join(crate_name);
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    root.to_string_lossy().into_owned()
}

/// Runs the cordis startup path: root → loader → plugins → wait for signal.
pub async fn run(options: &CliOptions) -> anyhow::Result<()> {
    // Install the exit handlers before any startup work: drivers that wait
    // for the ready marker may signal immediately after it appears, so the
    // handlers must already be registered by then (signals that arrive
    // earlier are queued by tokio and resolved below).
    let exit = ExitSignals::register();

    let config_path = options
        .config
        .clone()
        .unwrap_or_else(|| "cordis.yml".to_string());
    let plugins_dir = options
        .plugins_dir
        .clone()
        .unwrap_or_else(|| "plugins".to_string());

    let configs = parse_config(Path::new(&config_path))
        .map_err(|error| anyhow::anyhow!("cannot load config {config_path}: {error}"))?;

    let root = Context::new();
    let shared = std::env::var("CORDIS_SHARED").ok();
    let loader = Loader::with_shared(&root, shared);
    // Default console output.
    let _console = cordis_plugin_logger_console::install(
        &root,
        cordis_plugin_logger_console::ConsoleConfig {
            colors: 0,
            max_length: 10240,
            levels: None,
            show_diff: false,
            show_time: "".to_string(),
            label: None,
        },
    )
    .expect("console exporter");

    // Builtins (mirrors the TS loader's builtin table).
    loader.builtins.rcu(|builtins| {
        let mut next = (**builtins).clone();
        next.insert(
            "@cordisjs/plugin-include".to_string(),
            cordis_plugin_include::include_plugin(),
        );
        next.insert(
            "@cordisjs/plugin-hmr".to_string(),
            cordis_plugin_hmr::hmr_plugin(),
        );
        Arc::new(next)
    });

    // Load `.so` plugins from the plugins directory.
    // Keep the loaded libraries alive for the whole run: their plugin
    // instances (handles) are referenced by the registered plugins.
    let _libraries = load_so_plugins(&loader, Path::new(&plugins_dir))?;

    // Read the config into the tree and wait for everything to apply.
    loader.read(configs).await;
    loader.tree_handle().await_tree().await;
    // A plugin that failed to apply is a startup error.
    let failed: Vec<String> = loader
        .tree_handle()
        .entries()
        .into_iter()
        .filter_map(|entry| {
            let fiber = entry.fiber.lock().clone()?;
            if fiber.state() == cordis_core::FiberState::Failed {
                Some(format!(
                    "    at {base}#{id} (plugin {name})",
                    base = "cordis",
                    id = entry.id(),
                    name = entry.options.lock().name,
                ))
            } else {
                None
            }
        })
        .collect();
    if !failed.is_empty() {
        return Err(anyhow::anyhow!(
            "plugin failed to apply:\n{}",
            failed.join("\n")
        ));
    }
    loader
        .ctx
        .logger()
        .info(format!("cordis started (config {config_path})"));

    // Announce readiness only after the exit handlers are installed.
    eprintln!("cordis ready");
    if let Ok(marker) = std::env::var("CORDIS_READY_FILE") {
        let _ = std::fs::write(&marker, "ready");
    }
    exit.wait().await;
    loader.ctx.logger().info("cordis exiting");
    Ok(())
}

fn load_so_plugins(loader: &Loader, dir: &Path) -> anyhow::Result<Vec<SoPlugin>> {
    let mut libraries = Vec::new();
    if !dir.exists() {
        return Ok(libraries);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_plugin = path
            .extension()
            .is_some_and(|ext| ext == "so" || ext == "dylib");
        if !is_plugin {
            continue;
        }
        // SAFETY: the plugin is only used while the instance is alive and
        // never concurrently from two threads.
        let mut plugin = unsafe { SoPlugin::load(&path) }
            .map_err(|error| anyhow::anyhow!("cannot load plugin {}: {error}", path.display()))?;
        extern "C" fn cli_log(message: *const std::ffi::c_char) {
            // SAFETY: the plugin passes a NUL-terminated string.
            let text = unsafe { std::ffi::CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned();
            eprintln!("[plugin] {text}");
        }
        // SAFETY: the log callback stays valid for the plugin's lifetime.
        let handle = unsafe { plugin.create(cli_log) };
        if handle.is_null() {
            return Err(anyhow::anyhow!(
                "plugin create failed for {}",
                path.display()
            ));
        }
        let name = loader.register_so_plugin(&plugin).map_err(|error| {
            anyhow::anyhow!("cannot register plugin {}: {error}", path.display())
        })?;
        loader
            .ctx
            .logger()
            .info(format!("loaded plugin {name} ({})", path.display()));
        libraries.push(plugin);
    }
    Ok(libraries)
}

/// The registered exit signals (SIGINT/SIGTERM on unix, ctrl-c elsewhere).
struct ExitSignals {
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl ExitSignals {
    /// Registers the exit signals with the OS.
    fn register() -> Self {
        #[cfg(unix)]
        {
            Self {
                sigint: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("sigint handler"),
                sigterm: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("sigterm handler"),
            }
        }
        #[cfg(not(unix))]
        {
            ExitSignals
        }
    }

    /// Resolves when an exit signal arrives.
    async fn wait(self) {
        #[cfg(unix)]
        {
            let Self {
                mut sigint,
                mut sigterm,
            } = self;
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
