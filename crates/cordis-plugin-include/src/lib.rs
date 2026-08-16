//! Cordis include plugin (Rust port).
//!
//! Port of `@cordisjs/plugin-include`: reads yaml/json config files into an
//! entry tree and applies patches (story card D1).

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cordis_core::{Context, Effect, Plugin, sync_disposer};
use cordis_loader::{
    EntryGroup, EntryOptions, Loader, atomic_write, parse_config, serialize_config,
};
use serde::Deserialize;

/// The include plugin config (mirrors `Include.Config`).
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub struct IncludeConfig {
    /// The config file path (resolved against `baseUrl`).
    pub path: String,
    /// Initial config written when the file is missing.
    #[serde(default)]
    pub initial: Option<Vec<EntryOptions>>,
    /// Patches applied after reading.
    #[serde(default)]
    pub patches: Option<Vec<PatchOptions>>,
    /// Whether to log plugin activity.
    #[serde(default)]
    pub enable_logs: Option<bool>,
}

/// A single patch (mirrors `PatchOptions`).
#[derive(Clone, Debug, Deserialize, serde::Serialize, Default)]
pub struct PatchOptions {
    pub id: Option<String>,
    #[serde(default)]
    pub insert: Option<Vec<EntryOptions>>,
    pub name: Option<String>,
    #[serde(default)]
    pub config: Option<serde_yaml_ng::Value>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// The include plugin: registers a plugin that mounts a file-backed tree.
pub fn include_plugin() -> Plugin {
    Plugin {
        name: Some("include".to_string()),
        inject: vec![("loader".to_string(), None)],
        is_group: false,
        apply: Rc::new(|ctx: &Context, config: &Rc<dyn std::any::Any>| {
            let loader = ctx.get::<Loader>().expect("loader");
            let config = config
                .downcast_ref::<serde_yaml_ng::Value>()
                .and_then(|value| serde_yaml_ng::from_value::<IncludeConfig>(value.clone()).ok())
                .expect("include config");
            let fiber = ctx.fiber().clone();
            let entry = loader
                .entries()
                .into_iter()
                .find(|entry| {
                    entry
                        .fiber
                        .borrow()
                        .as_ref()
                        .map(|candidate| Rc::ptr_eq(candidate, &fiber))
                        .unwrap_or(false)
                })
                .expect("include entry");
            let subgroup = {
                let existing = entry.subgroup.borrow().clone();
                if let Some(subgroup) = existing {
                    subgroup
                } else {
                    let subgroup = EntryGroup::new(
                        loader.tree_handle(),
                        entry.ctx.clone(),
                        Some(entry.parent.clone()),
                    );
                    *subgroup.entry.borrow_mut() = Some(entry.clone());
                    *entry.subgroup.borrow_mut() = Some(subgroup.clone());
                    subgroup
                }
            };
            let loader = loader.clone();
            Effect::Async(Box::pin(async move {
                mount_include(&loader, &subgroup, &config, &entry).await;
                Ok(sync_disposer(|| {}))
            }))
        }),
    }
}

async fn mount_include(
    loader: &Loader,
    subgroup: &Rc<EntryGroup>,
    config: &IncludeConfig,
    entry: &cordis_loader::Entry,
) {
    let filename = resolve_path(&config.path);
    let content = fs::read_to_string(&filename);
    let data = match content {
        Ok(_content) => parse_config(&filename).unwrap_or_default(),
        Err(_) => {
            if let Some(initial) = &config.initial {
                let serialized = serialize_config(initial).unwrap_or_default();
                let _ = atomic_write(&filename, &serialized);
                parse_config(&filename).unwrap_or_default()
            } else {
                entry
                    .ctx
                    .logger()
                    .error(format!("config file not found: {}", filename.display()));
                Vec::new()
            }
        }
    };
    let patched = apply_patches(
        data,
        config.patches.as_deref().unwrap_or_default(),
        &entry.ctx,
    );
    loader.read_group(subgroup, patched).await;

    // Story card D5: tree writes are debounced to a single atomic file write
    // per event-loop turn, and `loader/config-update` fires on every write.
    let state = Rc::new(IncludeWriteState {
        filename,
        readonly: Cell::new(false),
        pending: Cell::new(false),
    });
    state.readonly.set(!check_writable(&state.filename));
    let loader_ctx = loader.ctx.clone();
    let tree = loader.tree_handle();
    let state_for_write = state.clone();
    let subgroup_for_write = subgroup.clone();
    *tree.write_callback.borrow_mut() = Some(Rc::new(move || {
        loader_ctx.emit("loader/config-update", &[]);
        if state_for_write.pending.get() {
            return;
        }
        state_for_write.pending.set(true);
        let state = state_for_write.clone();
        let subgroup = subgroup_for_write.clone();
        tokio::task::spawn_local(async move {
            // `yield_now` mirrors the TS `setTimeout(0)` debounce boundary:
            // writes issued in the same turn coalesce into one disk write.
            tokio::task::yield_now().await;
            let _ = state.write_once(&subgroup);
            state.pending.set(false);
        });
    }));
}

/// Refreshes the include entry whose config file is `filename` (F1: config
/// file changes trigger a reload of the include tree instead of an HMR).
pub async fn refresh_include_file(loader: &Loader, filename: &Path) -> bool {
    let canonical = match fs::canonicalize(filename) {
        Ok(path) => path,
        Err(_) => filename.to_path_buf(),
    };
    for entry in loader.tree_handle().entries() {
        let Some(config) = entry.options.borrow().config.clone() else {
            continue;
        };
        let Ok(config) = serde_yaml_ng::from_value::<IncludeConfig>(config) else {
            continue;
        };
        let candidate = fs::canonicalize(resolve_path(&config.path))
            .unwrap_or_else(|_| resolve_path(&config.path));
        if candidate != canonical {
            continue;
        }
        let Some(subgroup) = entry.subgroup.borrow().clone() else {
            continue;
        };
        if fs::read_to_string(&canonical).is_err() {
            continue;
        }
        let data = parse_config(&canonical).unwrap_or_default();
        let patched = apply_patches(
            data,
            config.patches.as_deref().unwrap_or_default(),
            &entry.ctx,
        );
        loader.read_group(&subgroup, patched).await;
        return true;
    }
    false
}

/// Shared per-instance state for the debounced write-back path.
struct IncludeWriteState {
    filename: PathBuf,
    readonly: Cell<bool>,
    pending: Cell<bool>,
}

impl IncludeWriteState {
    fn write_once(&self, subgroup: &Rc<EntryGroup>) -> Result<(), String> {
        if self.readonly.get() {
            return Err("cannot overwrite readonly config".to_string());
        }
        let entries: Vec<EntryOptions> = subgroup
            .entries
            .borrow()
            .iter()
            .map(|entry| entry.options.borrow().clone())
            .collect();
        let serialized = serialize_config(&entries)?;
        atomic_write(&self.filename, &serialized)
    }
}

/// Whether the config file is writable (mirrors the TS `checkAccess`).
fn check_writable(path: &Path) -> bool {
    fs::OpenOptions::new().append(true).open(path).is_ok()
}

fn resolve_path(path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn apply_patches(
    mut data: Vec<EntryOptions>,
    patches: &[PatchOptions],
    ctx: &Context,
) -> Vec<EntryOptions> {
    if patches.is_empty() {
        return data;
    }
    // id → (outer index, inner index for group children)
    let mut entry_map: std::collections::HashMap<String, (usize, Option<usize>)> =
        std::collections::HashMap::new();
    for (i, entry) in data.iter().enumerate() {
        if !entry.id.is_empty() {
            entry_map.insert(entry.id.clone(), (i, None));
        }
        if entry.group == Some(true)
            && let Some(Value::Sequence(children)) = &entry.config
        {
            let children: Vec<EntryOptions> =
                serde_yaml_ng::from_value(Value::Sequence(children.clone())).unwrap_or_default();
            for (j, child) in children.iter().enumerate() {
                if !child.id.is_empty() {
                    entry_map.insert(child.id.clone(), (i, Some(j)));
                }
            }
        }
    }

    for patch in patches {
        let (id, insert, name, overrides) = (
            patch.id.clone(),
            patch.insert.clone(),
            patch.name.clone(),
            patch,
        );
        if let Some(insert) = insert {
            if let Some(id) = &id {
                if let Some(&(target, inner)) = entry_map.get(id) {
                    if inner.is_none() && data[target].group == Some(true) {
                        let mut children: Vec<EntryOptions> = data[target]
                            .config
                            .as_ref()
                            .and_then(|value| serde_yaml_ng::from_value(value.clone()).ok())
                            .unwrap_or_default();
                        children.extend(insert);
                        data[target].config = Some(serde_yaml_ng::to_value(children).unwrap());
                    } else {
                        ctx.logger()
                            .warn(format!("patch insert: entry {id} is not a group"));
                    }
                } else {
                    ctx.logger()
                        .warn(format!("patch insert: entry {id} not found"));
                }
            } else {
                data.extend(insert);
            }
            continue;
        }

        let Some(id) = id else {
            ctx.logger()
                .warn("patch: id is required for non-insert patches");
            continue;
        };
        let Some(&(target, inner)) = entry_map.get(&id) else {
            ctx.logger().warn(format!("patch: entry {id} not found"));
            continue;
        };
        if let Some(inner) = inner {
            let mut children: Vec<EntryOptions> = data[target]
                .config
                .as_ref()
                .and_then(|value| serde_yaml_ng::from_value(value.clone()).ok())
                .unwrap_or_default();
            let mut child = children.get_mut(inner).cloned();
            data[target].config = Some(serde_yaml_ng::to_value(children).unwrap());
            if let Some(child) = &mut child {
                apply_overrides(child, name.as_deref(), overrides, ctx, &id);
            }
            if let Some(child) = child {
                let mut children: Vec<EntryOptions> = data[target]
                    .config
                    .as_ref()
                    .and_then(|value| serde_yaml_ng::from_value(value.clone()).ok())
                    .unwrap_or_default();
                if inner < children.len() {
                    children[inner] = child;
                }
                data[target].config = Some(serde_yaml_ng::to_value(children).unwrap());
            }
        } else {
            let Some(entry) = data.get_mut(target) else {
                continue;
            };
            apply_overrides(entry, name.as_deref(), overrides, ctx, &id);
        }
    }
    data
}

fn apply_overrides(
    entry: &mut EntryOptions,
    name: Option<&str>,
    overrides: &PatchOptions,
    ctx: &Context,
    id: &str,
) {
    if let Some(name) = name
        && name != entry.name
    {
        ctx.logger().warn(format!(
            "patch: name mismatch for {id} (expected {}, got {name}), skipping",
            entry.name
        ));
        return;
    }
    if let Some(config) = &overrides.config {
        entry.config = Some(config.clone());
    }
    if let Some(disabled) = overrides.disabled {
        entry.disabled = Some(disabled);
    }
}

use serde_yaml_ng::Value;
