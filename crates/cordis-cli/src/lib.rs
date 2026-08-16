//! cordis-cli startup path (story card H1).

use std::path::Path;
use std::rc::Rc;

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

/// Runs the cordis startup path: root → loader → plugins → wait for signal.
pub async fn run(options: &CliOptions) -> anyhow::Result<()> {
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
    // Default console output (D3).
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
    loader.builtins.borrow_mut().insert(
        "@cordisjs/plugin-include".to_string(),
        cordis_plugin_include::include_plugin(),
    );
    loader.builtins.borrow_mut().insert(
        "@cordisjs/plugin-hmr".to_string(),
        cordis_plugin_hmr::hmr_plugin(),
    );

    // Load `.so` plugins from the plugins directory (E3/E5 path).
    // Keep the loaded libraries alive for the whole run: their plugin
    // instances (handles) are referenced by the registered plugins.
    let _libraries = load_so_plugins(&loader, Path::new(&plugins_dir))?;

    // Read the config into the tree and wait for everything to apply.
    loader.read(configs).await;
    loader.tree_handle().await_tree().await;
    // H1.4: a plugin that failed to apply is a startup error.
    let failed: Vec<String> = loader
        .tree_handle()
        .entries()
        .into_iter()
        .filter_map(|entry| {
            let fiber = entry.fiber.borrow().clone()?;
            if fiber.state.get() == cordis_core::FiberState::Failed {
                Some(format!(
                    "    at {base}#{id} (plugin {name})",
                    base = "cordis",
                    id = entry.id(),
                    name = entry.options.borrow().name,
                ))
            } else {
                None
            }
        })
        .collect();
    if !failed.is_empty() {
        return Err(anyhow::anyhow!("plugin failed to apply:\n{}", failed.join("\n")));
    }
    loader.ctx.logger().info(format!("cordis started (config {config_path})"));

    // Signal handlers are installed inside `wait_for_exit`; announce that the
    // process is ready so drivers (tests/scripts) can signal it safely.
    eprintln!("cordis ready");
    if let Ok(marker) = std::env::var("CORDIS_READY_FILE") {
        let _ = std::fs::write(&marker, "ready");
    }
    wait_for_exit(&root).await;
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
        let is_plugin = path.extension().map(|ext| ext == "so" || ext == "dylib").unwrap_or(false);
        if !is_plugin {
            continue;
        }
        // SAFETY: the library is loaded on the host thread and used there.
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
            return Err(anyhow::anyhow!("plugin create failed for {}", path.display()));
        }
        let name = loader
            .register_so_plugin(&plugin)
            .map_err(|error| anyhow::anyhow!("cannot register plugin {}: {error}", path.display()))?;
        loader.ctx.logger().info(format!("loaded plugin {name} ({})", path.display()));
        libraries.push(plugin);
    }
    Ok(libraries)
}

async fn wait_for_exit(_root: &Context) {
    #[cfg(unix)]
    {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("sigint handler");
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler");
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

#[allow(dead_code)]
fn _keep_rc(_: Rc<()>) {}
