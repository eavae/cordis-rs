//! The Loader service.

use parking_lot::Mutex;
use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;

use cordis_core::{
    ApplyFn, Context, Effect, EventOptions, Fiber, Plugin, Service, event_callback,
    event_listener_async,
};

use crate::context_bridge::PluginHandlePtr;
use crate::entry::{
    Entry, EntryGroup, EntryOptions, EntryTree, PartialEntryOptions, StructuralChange, TreeState,
    current_group_chain, rebuild_group, rebuild_tree,
};
use crate::evaluator::EvalEnv;
use crate::so::SoPlugin;

/// The plugin loader service (mirrors `Loader` in loader/index.ts).
pub struct Loader {
    /// The context the loader was created on (its own scope).
    pub ctx: Context,
    /// The underlying entry tree.
    pub tree: Arc<EntryTree>,
    /// `CORDIS_SHARED` env data (mirrors `loader.envData`).
    pub env_data: serde_json::Value,
    /// The base url exposed to `!expr` as `base_url()` (defaults to the
    /// current working directory; mirrors `ctx.baseUrl` in the TS loader).
    base_url: Mutex<String>,
}

impl Service for Loader {
    const NAME: &'static str = "loader";
}

impl Deref for Loader {
    type Target = EntryTree;

    fn deref(&self) -> &Self::Target {
        &self.tree
    }
}

impl Loader {
    /// Creates a loader on `ctx`, provides `ctx.loader` and registers the
    /// internal hooks (write-back, reload log, self-dispose).
    pub fn new(ctx: &Context) -> Arc<Self> {
        let shared = std::env::var("CORDIS_SHARED").ok();
        Self::with_shared(ctx, shared)
    }

    /// Creates a loader with explicit `CORDIS_SHARED` data.
    pub fn with_shared(ctx: &Context, shared: Option<String>) -> Arc<Self> {
        let env_data = shared
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        let tree = EntryTree::new(ctx);
        let loader = Arc::new(Self {
            ctx: ctx.clone(),
            tree,
            env_data,
            base_url: Mutex::new(
                std::env::current_dir()
                    .map_or_else(|_| ".".to_string(), |path| path.display().to_string()),
            ),
        });
        let group_plugin = group_plugin(&loader);
        loader.tree.builtins.rcu(|builtins| {
            let mut next = (**builtins).clone();
            next.insert("@cordisjs/plugin-group".to_string(), group_plugin.clone());
            Arc::new(next)
        });

        // The loader service carries its availability check: while tasks are
        // pending under the `await` intercept, dependent fibers stay pending
        // (mirrors `ctx.reflect.provide('loader', this, this[Service.check])`).
        let loader_check = loader.clone();
        drop(
            ctx.provide_str_with_check(
                "loader",
                loader.clone(),
                Some(Arc::new(move |_ctx: &Context| loader_check.check())),
            )
            .unwrap(),
        );
        loader.register_internal_hooks();
        loader
    }

    /// The evaluation environment for `!expr` config expressions: the host
    /// platform, the loader's base url and the process environment.
    pub fn eval_env(&self) -> EvalEnv {
        EvalEnv {
            platform: platform_name(),
            base_url: self.base_url.lock().clone(),
            env_vars: std::env::vars().collect(),
        }
    }

    /// Overrides the base url used by `!expr` `base_url()`.
    pub fn set_base_url(&self, base_url: impl Into<String>) {
        *self.base_url.lock() = base_url.into();
    }

    /// Returns the underlying tree handle.
    pub fn tree_handle(&self) -> Arc<EntryTree> {
        Arc::clone(&self.tree)
    }

    fn register_internal_hooks(self: &Arc<Self>) {
        // internal/update write-back hook: plugin self-updates write back to
        // the entry options.
        let loader = self.clone();
        drop(
            self.ctx
                .on(
                    "internal/update",
                    event_listener_async(move |args, next| {
                        let loader = loader.clone();
                        async move {
                            let no_save = args[1].downcast_ref::<bool>().copied().unwrap_or(false);
                            let fiber = args[2].clone().downcast::<Fiber>().ok();
                            let next =
                                next.expect("internal/update listener invoked without `next`");
                            if let Some(fiber) = fiber
                                && !no_save
                                && let Some(entry) = loader.find_entry_for_fiber(&fiber)
                                && !loader.fiber_is_root_of(&entry, &fiber)
                                && let Ok(config) =
                                    args[0].clone().downcast::<serde_yaml_ng::Value>()
                            {
                                let mut options = entry.options.lock().clone();
                                options.config = Some((*config).clone());
                                *entry.options.lock() = options;
                                entry.tree.write();
                            }
                            let _ = next.next().await;
                            Ok(None)
                        }
                    }),
                    EventOptions {
                        prepend: true,
                        global: true,
                    },
                )
                .unwrap(),
        );

        // Reload log hook (global, non-prepend).
        let loader = self.clone();
        drop(
            self.ctx
                .on(
                    "internal/update",
                    event_listener_async(move |args, next| {
                        let loader = loader.clone();
                        async move {
                            let no_save = args[1].downcast_ref::<bool>().copied().unwrap_or(false);
                            let fiber = args[2].clone().downcast::<Fiber>().ok();
                            let next =
                                next.expect("internal/update listener invoked without `next`");
                            if !no_save
                                && let Some(fiber) = fiber
                                && let Some(entry) = loader.find_entry_for_fiber(&fiber)
                                && !loader.fiber_is_root_of(&entry, &fiber)
                            {
                                loader.show_log("reload", &entry);
                            }
                            let _ = next.next().await;
                            Ok(None)
                        }
                    }),
                    EventOptions {
                        prepend: false,
                        global: true,
                    },
                )
                .unwrap(),
        );

        // internal/plugin self-dispose hook.
        let loader = self.clone();
        drop(
            self.ctx
                .on(
                    "internal/plugin",
                    event_callback(move |args| {
                        let fiber = args[0].clone().downcast::<Fiber>().ok();
                        let Some(fiber) = fiber else {
                            return Ok(None);
                        };
                        // Merging entry-level inject happens on fiber
                        // creation; the entry is reachable from the root
                        // fiber's parent context.
                        if fiber.uid().is_some()
                            && let Some(entry) = loader.find_entry_for_parent(&fiber)
                        {
                            entry.merge_inject_into(&fiber);
                            return Ok(None);
                        }
                        // Only care about disposals (`uid` becomes None).
                        if fiber.uid().is_some() {
                            return Ok(None);
                        }
                        let Some(entry) = loader.find_entry_for_fiber(&fiber) else {
                            return Ok(None);
                        };
                        if entry.disabled() {
                            return Ok(None);
                        }
                        entry.options.lock().disabled = Some(true);
                        entry.tree.write();
                        Ok(None)
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
        );
    }

    fn find_entry_for_fiber(&self, fiber: &Arc<Fiber>) -> Option<Arc<Entry>> {
        self.tree.entries().into_iter().find(|entry| {
            entry
                .fiber
                .lock()
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, fiber))
        })
    }

    /// Finds the entry whose own context is `fiber`'s parent context (the
    /// entry's root fiber; mirrors `fiber.parent[Entry.key]`).
    fn find_entry_for_parent(&self, fiber: &Arc<Fiber>) -> Option<Arc<Entry>> {
        let parent = fiber.parent.as_ref()?;
        self.tree
            .entries()
            .into_iter()
            .find(|entry| parent.shares_inner(&entry.ctx))
    }

    /// Whether `fiber` is the root fiber of `entry` (mirrors
    /// `fiber.parent.fiber?.entry === fiber.entry`: child fibers under the
    /// same entry must not write back to the entry's config).
    fn fiber_is_root_of(&self, entry: &Arc<Entry>, fiber: &Arc<Fiber>) -> bool {
        let Some(root) = entry.fiber.lock().clone() else {
            return false;
        };
        fiber
            .parent
            .as_ref()
            .is_some_and(|parent| Arc::ptr_eq(parent.fiber(), &root))
    }

    /// The builtin group plugin: syncs the entry's subgroup from its config.
    fn group_plugin_inner(self: &Arc<Self>) -> Plugin {
        let loader = self.clone();
        Plugin {
            is_group: true,
            name: Some("group".to_string()),
            inject: Vec::new(),
            apply: Arc::new(move |ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                let fiber = ctx.fiber().clone();
                if let Some(entry) = loader.find_entry_for_fiber(&fiber) {
                    let configs: Vec<EntryOptions> = match config
                        .downcast_ref::<serde_yaml_ng::Value>()
                    {
                        Some(value) => serde_yaml_ng::from_value(value.clone()).unwrap_or_default(),
                        None => Vec::new(),
                    };
                    let subgroup = {
                        let existing = entry.subgroup();
                        if let Some(subgroup) = existing {
                            subgroup
                        } else {
                            loader.tree_handle().attach_subgroup(&entry)
                        }
                    };
                    // `Service.init` registers the stop disposer first.
                    // The disposer resolves the current subgroup node at
                    // stop time: structural snapshots are immutable, so the
                    // captured handle may be stale.
                    let stop_subgroup = subgroup.clone();
                    let stop_loader = loader.clone();
                    let _ = ctx.fiber().effect(
                        move || {
                            Effect::Disposer(cordis_core::sync_disposer(move || {
                                let subgroup =
                                    stop_loader.tree_handle().current_group(&stop_subgroup);
                                let entries: Vec<Arc<Entry>> =
                                    subgroup.entries.iter().cloned().collect();
                                for entry in entries {
                                    let fiber = entry.fiber.lock().clone();
                                    if let Some(fiber) = fiber {
                                        tokio::task::spawn_local(fiber.dispose());
                                    }
                                    *entry.fiber.lock() = None;
                                }
                            }))
                        },
                        "group.stop()",
                    );
                    let loader = loader.clone();
                    Effect::Async(Box::pin(async move {
                        loader.read_group(&subgroup, configs).await;
                        Ok(cordis_core::sync_disposer(|| {}))
                    }))
                } else {
                    Effect::None
                }
            }),
        }
    }

    /// Reads a config list and reconciles the tree (mirrors `tree.read`).
    pub async fn read(&self, configs: Vec<EntryOptions>) {
        let root = self.tree.state.load_full().root.clone();
        self.read_group(&root, configs).await;
    }

    /// Reconciles `configs` against the entries of `group`, creating,
    /// updating and disposing entries as needed (mirrors `tree.read` applied
    /// to a subgroup).
    pub async fn read_group(&self, group: &Arc<EntryGroup>, configs: Vec<EntryOptions>) {
        // Structural phase: reconcile membership in a single atomic commit.
        let (updates, created, removed, to_init) = {
            let mut updates: Vec<(Arc<Entry>, PartialEntryOptions)> = Vec::new();
            let mut created: Vec<Arc<Entry>> = Vec::new();
            let mut removed: Vec<Arc<Entry>> = Vec::new();
            let mut to_init: Vec<Arc<Entry>> = Vec::new();
            self.tree.state.rcu(|old| {
                updates.clear();
                created.clear();
                removed.clear();
                to_init.clear();
                let (group, chain) = current_group_chain(old, group).expect("group");
                let mut next_entries: Vec<Arc<Entry>> = Vec::new();
                for options in &configs {
                    if options.group == Some(true) {
                        let mut options = options.clone();
                        self.tree.ensure_id(&mut options);
                        if let Some(existing) = self.find_matching(&group, &options) {
                            updates.push((
                                existing.clone(),
                                PartialEntryOptions::from_options(&options),
                            ));
                            next_entries.push(existing);
                        } else {
                            let entry =
                                Entry::new(self.tree_handle(), group.clone(), options.clone());
                            created.push(entry.clone());
                            next_entries.push(entry);
                        }
                        continue;
                    }
                    if let Some(existing) = self.find_matching(&group, options) {
                        updates
                            .push((existing.clone(), PartialEntryOptions::from_options(options)));
                        next_entries.push(existing);
                    } else {
                        let mut options = options.clone();
                        self.tree.ensure_id(&mut options);
                        let entry = Entry::new(self.tree_handle(), group.clone(), options.clone());
                        created.push(entry.clone());
                        next_entries.push(entry);
                    }
                }
                let local_removed: Vec<Arc<Entry>> = group
                    .entries
                    .iter()
                    .filter(|entry| !next_entries.iter().any(|next| Arc::ptr_eq(next, entry)))
                    .cloned()
                    .collect();
                let mut final_entries: Vec<Arc<Entry>> = group
                    .entries
                    .iter()
                    .filter(|entry| !local_removed.iter().any(|gone| Arc::ptr_eq(gone, entry)))
                    .cloned()
                    .collect();
                for entry in &created {
                    final_entries.push(entry.clone());
                }
                let final_entries = Arc::new(final_entries);
                let change = StructuralChange {
                    chain,
                    apply: Box::new(move |node| {
                        rebuild_group(node, final_entries.clone(), node.subgroups.clone())
                    }),
                };
                let mut parent_of = old.parent_of.clone();
                for entry in &local_removed {
                    parent_of.remove(&entry.key);
                }
                let new_root = rebuild_tree(&old.root, &[change], &mut parent_of);
                removed.extend(local_removed);
                to_init.extend(
                    next_entries
                        .iter()
                        .filter(|entry| entry.fiber.lock().is_none())
                        .cloned(),
                );
                Arc::new(TreeState {
                    root: new_root,
                    parent_of,
                })
            });
            (updates, created, removed, to_init)
        };

        // Post-commit phase (no lock): option updates may run user callbacks,
        // removed fibers are disposed as counted tasks, new entries init.
        for (entry, options) in updates {
            entry.update(options, false, true);
        }
        for entry in &created {
            let options = PartialEntryOptions::from_options(&entry.options.lock().clone());
            entry.update(options, true, false);
        }
        for entry in &removed {
            let fiber = entry.detach_and_clear_fiber();
            if let Some(fiber) = fiber {
                self.tree.spawn_dispose(fiber);
            }
        }
        for entry in to_init {
            entry.init().await;
        }
    }

    /// Finds an existing entry by id, or by name when the new id is
    /// auto-generated (keeps structure stable across re-reads).
    fn find_matching(&self, group: &Arc<EntryGroup>, options: &EntryOptions) -> Option<Arc<Entry>> {
        group
            .entries
            .iter()
            .find(|entry| {
                if !options.id.is_empty() {
                    entry.options.lock().id == options.id
                } else {
                    entry.options.lock().name == options.name
                        && entry.options.lock().group == options.group
                }
            })
            .cloned()
    }

    /// Registers a mock plugin under a name (test helper, mirrors `mock`).
    pub fn mock(&self, name: &str, apply: ApplyFn) -> String {
        self.mock_with_inject(name, Vec::new(), apply)
    }

    /// Registers a mock plugin with an inject list (test helper).
    pub fn mock_with_inject(&self, name: &str, inject: Vec<String>, apply: ApplyFn) -> String {
        let plugin = Plugin {
            is_group: false,
            name: None,
            inject: inject.into_iter().map(|name| (name, None)).collect(),
            apply,
        };
        self.tree.plugins.rcu(|plugins| {
            let mut next = (**plugins).clone();
            next.insert(name.to_string(), plugin.clone());
            Arc::new(next)
        });
        name.to_string()
    }

    /// Registers a loaded `.so` plugin into the tree under its metadata
    /// name. The plugin's config validator and apply entry are bridged into
    /// the core [`Plugin`].
    pub fn register_so_plugin(&self, plugin: &SoPlugin) -> Result<String, String> {
        let metadata = plugin
            .metadata()
            .ok_or_else(|| "plugin does not export plugin_meta".to_string())??;
        let validate = plugin.validator();
        let apply_entry = plugin.apply_fn();
        let handle = plugin
            .handle_ptr()
            .ok_or_else(|| "plugin instance is not created".to_string())?;
        let handle = PluginHandlePtr(handle);
        let name = metadata.name.clone();
        let name_for_error = name.clone();
        let apply: ApplyFn = Arc::new(move |ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
            let handle = handle.get();
            let config = config
                .downcast_ref::<serde_yaml_ng::Value>()
                .cloned()
                .unwrap_or(serde_yaml_ng::Value::Null);
            let json = serde_json::to_string(&config).unwrap_or_else(|_| "null".to_string());
            let json = std::ffi::CString::new(json).expect("config has no NUL");
            if let Some(validate) = validate
                && unsafe { validate(json.as_ptr()) } != 0
            {
                return Effect::Error(format!("config rejected by plugin {name_for_error}").into());
            }
            if let Some(apply_entry) = apply_entry {
                if !crate::context_bridge::is_handle_live(handle) {
                    return Effect::Error(
                        format!("plugin {name_for_error} instance is disposed").into(),
                    );
                }
                // SAFETY: the handle came from plugin_create and stays valid
                // while the owning SoPlugin is alive (held by the tree).
                // The session binds the handle to this fiber's context for
                // the duration of the call.
                crate::context_bridge::with_session(handle, ctx, || {
                    // SAFETY: handle and vtable are valid (live check above).
                    unsafe { apply_entry(handle, json.as_ptr()) };
                });
            }
            Effect::None
        });
        let plugin = Plugin {
            is_group: false,
            name: Some(name.clone()),
            inject: metadata
                .inject
                .iter()
                .map(|name| (name.clone(), None))
                .collect(),
            apply,
        };
        self.tree.plugins.rcu(|plugins| {
            let mut next = (**plugins).clone();
            next.insert(name.clone(), plugin.clone());
            Arc::new(next)
        });
        Ok(name)
    }

    /// The fiber of the entry with the given id (test helper).
    pub fn expect_fiber(&self, id: &str) -> Arc<Fiber> {
        self.tree
            .entries()
            .into_iter()
            .find(|entry| entry.id() == id)
            .and_then(|entry| entry.fiber.lock().clone())
            .expect("entry fiber")
    }

    /// The raw entry data (test helper, mirrors `loader.data`).
    pub fn data(&self) -> Vec<EntryOptions> {
        self.tree
            .entries()
            .iter()
            .map(|entry| entry.options.lock().clone())
            .collect()
    }

    /// Locates the entry id owning `fiber` (mirrors `loader.locate`).
    pub fn locate(&self, fiber: &Arc<Fiber>) -> Option<String> {
        self.tree.entries().into_iter().find_map(|entry| {
            let matches = entry
                .fiber
                .lock()
                .as_ref()
                .is_some_and(|candidate| Arc::ptr_eq(candidate, fiber));
            if matches { Some(entry.id()) } else { None }
        })
    }

    /// The loader's service check: unavailable while tasks are pending when
    /// the `await` intercept is enabled (mirrors `Loader.check`).
    pub fn check(&self) -> bool {
        let await_config = self
            .ctx
            .resolve_config::<LoaderIntercept>("loader", None, None)
            .await_enabled;
        !(await_config && self.tree.get_tasks() > 0)
    }

    /// Logs an apply/reload message when logs are enabled.
    pub fn show_log(&self, r#type: &str, entry: &Entry) {
        let is_group = entry.options.lock().group == Some(true);
        if is_group || !self.enable_logs {
            return;
        }
        self.ctx
            .logger()
            .named("loader")
            .info(format!("{type} plugin {}", entry.options.lock().name));
    }
}

/// Returns the builtin group plugin bound to `loader` (the Rust counterpart
/// of the `Group` class in the TS loader). Re-exported by
/// `cordis-plugin-group`, mirroring the TS `@cordisjs/plugin-group` package.
pub fn group_plugin(loader: &Arc<Loader>) -> Plugin {
    loader.group_plugin_inner()
}

/// The TS-compatible platform name used by `!expr` (`darwin`/`win32`/`linux`).
fn platform_name() -> String {
    match std::env::consts::OS {
        "macos" => "darwin".to_string(),
        "windows" => "win32".to_string(),
        other => other.to_string(),
    }
}

/// The `loader` intercept config (mirrors `Loader.Intercept`).
#[derive(Clone, Debug, Default)]
pub struct LoaderIntercept {
    await_enabled: bool,
}

impl LoaderIntercept {
    /// An intercept that makes the loader unavailable while tasks are pending.
    pub fn awaiting() -> Self {
        Self {
            await_enabled: true,
        }
    }
}

impl cordis_core::Config for LoaderIntercept {
    fn merge(&self, other: &Self) -> Self {
        Self {
            await_enabled: other.await_enabled || self.await_enabled,
        }
    }
}
