//! The Loader service.

use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use cordis_core::{AnyNext, ApplyFn, Context, Effect, EventOptions, Fiber, Plugin, Service};

use crate::entry::{Entry, EntryGroup, EntryOptions, EntryTree, PartialEntryOptions};
use crate::evaluator::EvalEnv;
use crate::so::SoPlugin;

/// The plugin loader service (mirrors `Loader` in loader/index.ts).
pub struct Loader {
    pub ctx: Context,
    pub tree: Rc<EntryTree>,
    /// `CORDIS_SHARED` env data (mirrors `loader.envData`).
    pub env_data: serde_json::Value,
    /// The base url exposed to `!expr` as `base_url()` (defaults to the
    /// current working directory; mirrors `ctx.baseUrl` in the TS loader).
    base_url: RefCell<String>,
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
    pub fn new(ctx: &Context) -> Rc<Self> {
        let shared = std::env::var("CORDIS_SHARED").ok();
        Self::with_shared(ctx, shared)
    }

    /// Creates a loader with explicit `CORDIS_SHARED` data.
    pub fn with_shared(ctx: &Context, shared: Option<String>) -> Rc<Self> {
        let env_data = shared
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .unwrap_or(serde_json::json!({}));
        let tree = EntryTree::new(ctx);
        let loader = Rc::new(Loader {
            ctx: ctx.clone(),
            tree,
            env_data,
            base_url: RefCell::new(
                std::env::current_dir()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| ".".to_string()),
            ),
        });
        let group_plugin = group_plugin(&loader);
        loader
            .tree
            .builtins
            .borrow_mut()
            .insert("@cordisjs/plugin-group".to_string(), group_plugin);

        // The loader service carries its availability check: while tasks are
        // pending under the `await` intercept, dependent fibers stay pending
        // (mirrors `ctx.reflect.provide('loader', this, this[Service.check])`).
        let loader_check = loader.clone();
        drop(
            ctx.provide_str_with_check(
                "loader",
                loader.clone(),
                Some(Rc::new(move |_ctx: &Context| loader_check.check())),
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
            base_url: self.base_url.borrow().clone(),
            env_vars: std::env::vars().collect(),
        }
    }

    /// Overrides the base url used by `!expr` `base_url()`.
    pub fn set_base_url(&self, base_url: impl Into<String>) {
        *self.base_url.borrow_mut() = base_url.into();
    }

    /// Returns the underlying tree handle.
    pub fn tree_handle(&self) -> Rc<EntryTree> {
        Rc::clone(&self.tree)
    }

    fn register_internal_hooks(self: &Rc<Self>) {
        // internal/update write-back hook: plugin self-updates write back to
        // the entry options.
        let loader = self.clone();
        drop(
            self.ctx
                .on(
                    "internal/update",
                    Rc::new(move |args| {
                        let no_save = args[1].downcast_ref::<bool>().copied().unwrap_or(false);
                        let fiber = args[2].clone().downcast::<Fiber>().ok();
                        let next = &args[3].downcast_ref::<AnyNext>().expect("next").0;
                        if let Some(fiber) = fiber
                            && !no_save
                            && let Some(entry) = loader.find_entry_for_fiber(&fiber)
                            && !loader.fiber_is_root_of(&entry, &fiber)
                            && let Ok(config) = args[0].clone().downcast::<serde_yaml_ng::Value>()
                        {
                            let mut options = entry.options.borrow().clone();
                            options.config = Some((*config).clone());
                            *entry.options.borrow_mut() = options;
                            entry.tree.write();
                        }
                        next();
                        Ok(None)
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
                    Rc::new(move |args| {
                        let no_save = args[1].downcast_ref::<bool>().copied().unwrap_or(false);
                        let fiber = args[2].clone().downcast::<Fiber>().ok();
                        let next = &args[3].downcast_ref::<AnyNext>().expect("next").0;
                        if !no_save
                            && let Some(fiber) = fiber
                            && let Some(entry) = loader.find_entry_for_fiber(&fiber)
                            && !loader.fiber_is_root_of(&entry, &fiber)
                        {
                            loader.show_log("reload", &entry);
                        }
                        next();
                        Ok(None)
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
                    Rc::new(move |args| {
                        let fiber = args[0].clone().downcast::<Fiber>().ok();
                        let Some(fiber) = fiber else {
                            return Ok(None);
                        };
                        // Merging entry-level inject happens on fiber
                        // creation; the entry is reachable from the root
                        // fiber's parent context.
                        if fiber.uid.get().is_some()
                            && let Some(entry) = loader.find_entry_for_parent(&fiber)
                        {
                            entry.merge_inject_into(&fiber);
                            return Ok(None);
                        }
                        // Only care about disposals (`uid` becomes None).
                        if fiber.uid.get().is_some() {
                            return Ok(None);
                        }
                        let Some(entry) = loader.find_entry_for_fiber(&fiber) else {
                            return Ok(None);
                        };
                        if entry.disabled() {
                            return Ok(None);
                        }
                        entry.options.borrow_mut().disabled = Some(true);
                        entry.tree.write();
                        Ok(None)
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
        );
    }

    fn find_entry_for_fiber(&self, fiber: &Rc<Fiber>) -> Option<Rc<Entry>> {
        self.tree.entries().into_iter().find(|entry| {
            entry
                .fiber
                .borrow()
                .as_ref()
                .map(|candidate| Rc::ptr_eq(candidate, fiber))
                .unwrap_or(false)
        })
    }

    /// Finds the entry whose own context is `fiber`'s parent context (the
    /// entry's root fiber; mirrors `fiber.parent[Entry.key]`).
    fn find_entry_for_parent(&self, fiber: &Rc<Fiber>) -> Option<Rc<Entry>> {
        let parent = fiber.parent.as_ref()?;
        self.tree
            .entries()
            .into_iter()
            .find(|entry| parent.shares_inner(&entry.ctx.borrow()))
    }

    /// Whether `fiber` is the root fiber of `entry` (mirrors
    /// `fiber.parent.fiber?.entry === fiber.entry`: child fibers under the
    /// same entry must not write back to the entry's config).
    fn fiber_is_root_of(&self, entry: &Rc<Entry>, fiber: &Rc<Fiber>) -> bool {
        let Some(root) = entry.fiber.borrow().clone() else {
            return false;
        };
        fiber
            .parent
            .as_ref()
            .map(|parent| Rc::ptr_eq(parent.fiber(), &root))
            .unwrap_or(false)
    }

    /// The builtin group plugin: syncs the entry's subgroup from its config.
    fn group_plugin_inner(self: &Rc<Self>) -> Plugin {
        let loader = self.clone();
        Plugin {
            is_group: true,
            name: Some("group".to_string()),
            inject: Vec::new(),
            apply: Rc::new(move |ctx: &Context, config: &Rc<dyn std::any::Any>| {
                let fiber = ctx.fiber().clone();
                if let Some(entry) = loader.find_entry_for_fiber(&fiber) {
                    let configs: Vec<EntryOptions> = match config
                        .downcast_ref::<serde_yaml_ng::Value>()
                    {
                        Some(value) => serde_yaml_ng::from_value(value.clone()).unwrap_or_default(),
                        None => Vec::new(),
                    };
                    let subgroup = {
                        let existing = entry.subgroup.borrow().clone();
                        if let Some(subgroup) = existing {
                            subgroup
                        } else {
                            let subgroup = EntryGroup::new(
                                loader.tree_handle(),
                                entry.ctx.borrow().clone(),
                                Some(entry.parent.borrow().clone()),
                            );
                            *subgroup.entry.borrow_mut() = Some(entry.clone());
                            *entry.subgroup.borrow_mut() = Some(subgroup.clone());
                            subgroup
                        }
                    };
                    // `Service.init` registers the stop disposer first.
                    let stop_subgroup = subgroup.clone();
                    let _ = ctx.fiber().effect(
                        move || {
                            Effect::Disposer(cordis_core::sync_disposer(move || {
                                let entries: Vec<Rc<Entry>> =
                                    stop_subgroup.entries.borrow().clone();
                                for entry in entries {
                                    let fiber = entry.fiber.borrow().clone();
                                    if let Some(fiber) = fiber {
                                        tokio::task::spawn_local(fiber.dispose());
                                    }
                                    *entry.fiber.borrow_mut() = None;
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
        let root = self.tree.root.borrow().clone().expect("root");
        self.read_group(&root, configs).await;
    }

    pub async fn read_group(&self, group: &Rc<EntryGroup>, configs: Vec<EntryOptions>) {
        let mut next_entries: Vec<Rc<Entry>> = Vec::new();
        for options in configs {
            if options.group == Some(true) {
                let mut options = options;
                self.tree.ensure_id(&mut options);
                // Group entry: ensure the entry and its subgroup, then process
                // the nested config.
                let entry = if let Some(existing) = self.find_matching(group, &options) {
                    existing.update(PartialEntryOptions::from_options(&options), false, true);
                    existing
                } else {
                    let entry = Entry::new(self.tree_handle(), group.clone(), options.clone());
                    entry.update(PartialEntryOptions::from_options(&options), true, false);
                    group.entries.borrow_mut().push(entry.clone());
                    entry
                };
                next_entries.push(entry.clone());
                if entry.fiber.borrow().is_none() && !entry.disabled() {
                    entry.init().await;
                }
                continue;
            }
            if let Some(existing) = self.find_matching(group, &options) {
                existing.update(PartialEntryOptions::from_options(&options), false, true);
                next_entries.push(existing);
            } else {
                let mut options = options;
                self.tree.ensure_id(&mut options);
                let entry = Entry::new(self.tree_handle(), group.clone(), options.clone());
                entry.update(PartialEntryOptions::from_options(&options), true, false);
                group.entries.borrow_mut().push(entry.clone());
                next_entries.push(entry);
            }
        }
        let removed: Vec<Rc<Entry>> = group
            .entries
            .borrow()
            .iter()
            .filter(|entry| !next_entries.iter().any(|next| Rc::ptr_eq(next, entry)))
            .cloned()
            .collect();
        for entry in removed {
            if let Some(fiber) = entry.fiber.borrow().clone() {
                tokio::task::spawn_local(fiber.dispose());
            }
            group
                .entries
                .borrow_mut()
                .retain(|item| !Rc::ptr_eq(item, &entry));
        }
        for entry in next_entries
            .into_iter()
            .filter(|entry| entry.fiber.borrow().is_none())
        {
            entry.init().await;
        }
    }

    /// Finds an existing entry by id, or by name when the new id is
    /// auto-generated (keeps structure stable across re-reads).
    fn find_matching(&self, group: &Rc<EntryGroup>, options: &EntryOptions) -> Option<Rc<Entry>> {
        group
            .entries
            .borrow()
            .iter()
            .find(|entry| {
                if !options.id.is_empty() {
                    entry.options.borrow().id == options.id
                } else {
                    entry.options.borrow().name == options.name
                        && entry.options.borrow().group == options.group
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
        self.tree.plugins.borrow_mut().insert(
            name.to_string(),
            Plugin {
                is_group: false,
                name: None,
                inject: inject.into_iter().map(|name| (name, None)).collect(),
                apply,
            },
        );
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
        let name = metadata.name.clone();
        let name_for_error = name.clone();
        let apply: ApplyFn = Rc::new(move |ctx: &Context, config: &Rc<dyn std::any::Any>| {
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
        self.tree.plugins.borrow_mut().insert(name.clone(), plugin);
        Ok(name)
    }

    /// The fiber of the entry with the given id (test helper).
    pub fn expect_fiber(&self, id: &str) -> Rc<Fiber> {
        self.tree
            .entries()
            .into_iter()
            .find(|entry| entry.id() == id)
            .and_then(|entry| entry.fiber.borrow().clone())
            .expect("entry fiber")
    }

    /// The raw entry data (test helper, mirrors `loader.data`).
    pub fn data(&self) -> Vec<EntryOptions> {
        self.tree
            .entries()
            .iter()
            .map(|entry| entry.options.borrow().clone())
            .collect()
    }

    /// Locates the entry id owning `fiber` (mirrors `loader.locate`).
    pub fn locate(&self, fiber: &Rc<Fiber>) -> Option<String> {
        self.tree.entries().into_iter().find_map(|entry| {
            let matches = entry
                .fiber
                .borrow()
                .as_ref()
                .map(|candidate| Rc::ptr_eq(candidate, fiber))
                .unwrap_or(false);
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
        if entry.options.borrow().group == Some(true) || !self.enable_logs {
            return;
        }
        self.ctx
            .logger()
            .named("loader")
            .info(format!("{type} plugin {}", entry.options.borrow().name));
    }
}

/// Returns the builtin group plugin bound to `loader` (the Rust counterpart
/// of the `Group` class in the TS loader). Re-exported by
/// `cordis-plugin-group`, mirroring the TS `@cordisjs/plugin-group` package.
pub fn group_plugin(loader: &Rc<Loader>) -> Plugin {
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
        LoaderIntercept {
            await_enabled: true,
        }
    }
}

impl cordis_core::Config for LoaderIntercept {
    fn merge(&self, other: &Self) -> Self {
        LoaderIntercept {
            await_enabled: other.await_enabled || self.await_enabled,
        }
    }
}
