//! Cordis HMR plugin.
//!
//! File watching with debounce and ignored globs; config files owned by the
//! include plugin are refreshed instead of triggering a reload.

pub mod build;
pub mod graph;
pub mod reload;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cordis_core::{Context, Effect, Service, sync_disposer};
use cordis_loader::Loader;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;

/// HMR config (mirrors `Hmr.Config` defaults).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct HmrConfig {
    /// Base directory for resolving `root` and ignored globs.
    #[serde(default)]
    pub base: Option<String>,
    /// Directories to watch.
    #[serde(default = "default_root")]
    pub root: Vec<String>,
    /// Debounce window in milliseconds.
    #[serde(default = "default_debounce")]
    pub debounce: u64,
    /// Glob patterns to ignore (relative to `base`).
    #[serde(default = "default_ignored")]
    pub ignored: Vec<String>,
}

fn default_root() -> Vec<String> {
    vec![".".to_string()]
}

fn default_debounce() -> u64 {
    100
}

fn default_ignored() -> Vec<String> {
    vec![
        "**/node_modules".to_string(),
        "**/.*".to_string(),
        "cache".to_string(),
        "data".to_string(),
    ]
}

impl Default for HmrConfig {
    fn default() -> Self {
        HmrConfig {
            base: None,
            root: default_root(),
            debounce: default_debounce(),
            ignored: default_ignored(),
        }
    }
}

/// The HMR service (mirrors the `Hmr` plugin in the TS reference).
pub struct HmrService {
    /// The HMR configuration.
    pub config: HmrConfig,
}

impl Service for HmrService {
    const NAME: &'static str = "hmr";
}

/// Registers the HMR plugin: watches `config.root` and routes changes.
pub fn hmr_plugin() -> cordis_core::Plugin {
    use cordis_core::Plugin;
    Plugin {
        is_group: false,
        name: Some("hmr".to_string()),
        inject: vec![("loader".to_string(), None)],
        apply: Rc::new(|ctx: &Context, config: &Rc<dyn std::any::Any>| {
            let loader = ctx.get::<Loader>().expect("loader");
            let config = config
                .downcast_ref::<serde_yaml_ng::Value>()
                .and_then(|value| serde_yaml_ng::from_value::<HmrConfig>(value.clone()).ok())
                .unwrap_or_default();
            let base_dir = config
                .base
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let watcher = FileWatcher::start(ctx.clone(), loader, config.clone(), base_dir);
            Effect::Disposer(sync_disposer(move || {
                watcher.stop();
            }))
        }),
    }
}

/// The running file watcher.
pub struct FileWatcher {
    handle: Option<tokio::task::JoinHandle<()>>,
    watcher: Rc<RefCell<Option<RecommendedWatcher>>>,
    watched: Vec<PathBuf>,
}

impl FileWatcher {
    /// Starts watching; returns a handle that stops the watcher on drop.
    pub fn start(
        ctx: Context,
        loader: Rc<Loader>,
        config: HmrConfig,
        base_dir: PathBuf,
    ) -> Rc<FileWatcher> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<PathBuf>(64);
        let watcher = match RecommendedWatcher::new(
            tx,
            notify::Config::default().with_poll_interval(std::time::Duration::from_millis(500)),
        ) {
            Ok(watcher) => watcher,
            Err(error) => {
                ctx.logger().error(format!("cannot start watcher: {error}"));
                return Rc::new(FileWatcher {
                    handle: None,
                    watcher: Rc::new(RefCell::new(None)),
                    watched: Vec::new(),
                });
            }
        };
        let watcher = Rc::new(RefCell::new(Some(watcher)));
        let mut watched = Vec::new();
        for root in &config.root {
            let path = base_dir.join(root);
            if let Some(watcher) = watcher.borrow_mut().as_mut() {
                if let Err(error) = watcher.watch(&path, RecursiveMode::Recursive) {
                    ctx.logger()
                        .warn(format!("cannot watch {}: {error}", path.display()));
                } else {
                    watched.push(path);
                }
            }
        }

        let ignored = compile_ignored(&config.ignored, &base_dir);
        let debounce = config.debounce.max(1);
        // The notify channel is blocking; forward to a tokio channel on a
        // dedicated thread so the single-threaded runtime never blocks.
        std::thread::spawn(move || {
            while let Ok(Ok(notify::Event { paths, .. })) = rx.recv() {
                for path in paths {
                    let _ = event_tx.blocking_send(path);
                }
            }
        });
        let handle = tokio::task::spawn_local(async move {
            let mut pending: Option<PathBuf>;
            while let Some(path) = event_rx.recv().await {
                if ignored(&path) {
                    continue;
                }
                pending = Some(path);
                tokio::time::sleep(std::time::Duration::from_millis(debounce)).await;
                if let Some(path) = pending.take() {
                    route_change(&ctx, &loader, &path).await;
                }
            }
        });

        Rc::new(FileWatcher {
            handle: Some(handle),
            watcher,
            watched,
        })
    }

    /// Stops the watcher.
    pub fn stop(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
        if let Some(watcher) = self.watcher.borrow_mut().as_mut() {
            for path in &self.watched {
                let _ = watcher.unwatch(path);
            }
        }
    }
}

fn compile_ignored(patterns: &[String], base: &Path) -> Rc<dyn Fn(&Path) -> bool> {
    let patterns = patterns.to_vec();
    let base = base.to_path_buf();
    Rc::new(move |path: &Path| {
        let relative = path.strip_prefix(&base).unwrap_or(path);
        let components: Vec<String> = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        patterns.iter().any(|pattern| {
            if pattern == "**/node_modules" {
                components.iter().any(|segment| segment == "node_modules")
            } else if pattern == "**/.*" {
                components.iter().any(|segment| segment.starts_with('.'))
            } else {
                components.iter().any(|segment| segment == pattern)
            }
        })
    })
}

async fn route_change(ctx: &Context, loader: &Loader, path: &Path) {
    // Config files owned by the include plugin are refreshed, not reloaded.
    if cordis_plugin_include::refresh_include_file(loader, path).await {
        return;
    }
    ctx.emit(
        "hmr/change",
        &[Rc::new(path.to_string_lossy().into_owned())],
    );
}

/// Validates an HMR config (defaults + field shape).
pub fn validate_config(config: &HmrConfig) -> Result<(), String> {
    if config.debounce == 0 {
        return Err(validate_message("en-US", "debounce"));
    }
    Ok(())
}

/// HMR config validation messages (static string table; en-US/zh-CN).
pub fn validate_message(locale: &str, field: &str) -> String {
    match (locale, field) {
        ("en-US", "debounce") => "hmr.config.debounce: must be a positive integer".to_string(),
        ("zh-CN", "debounce") => "hmr.config.debounce: 必须为正整数".to_string(),
        ("en-US", "root") => "hmr.config.root: must be a non-empty array".to_string(),
        ("zh-CN", "root") => "hmr.config.root: 不能为空数组".to_string(),
        ("en-US", _) => format!("hmr.config.{field}: invalid value"),
        ("zh-CN", _) => format!("hmr.config.{field}: 配置无效"),
        (_, _) => format!("hmr.config.{field}: invalid value"),
    }
}
