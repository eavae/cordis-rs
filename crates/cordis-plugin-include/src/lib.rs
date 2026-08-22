//! Cordis include plugin (Rust port).
//!
//! Port of `@cordisjs/plugin-include`: reads yaml/json config files into an
//! entry tree and applies patches.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cordis_core::{Context, Effect, Plugin, sync_disposer};
use cordis_loader::{
    EntryGroup, EntryOptions, IsolateValue, Loader, atomic_write, parse_config, serialize_config,
};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value;

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

/// A patch field value: `Set(v)` overrides the target field, `Clear` sets it
/// to null/absent (mirrors `target[key] = value` with `value === null`), and
/// `Absent` leaves the field untouched.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Override<T> {
    /// The patch does not touch this field.
    #[default]
    Absent,
    /// Overrides the field with `v`.
    Set(T),
    /// Clears the field (sets it to null/absent).
    Clear,
}

impl<T> Override<T> {
    /// Whether the patch does not touch this field.
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }

    /// The resulting `Option`: `Set` keeps the value, everything else clears.
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Set(value) => Some(value),
            _ => None,
        }
    }
}

impl<T: Serialize> Serialize for Override<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Absent => serializer.serialize_none(),
            Self::Set(value) => value.serialize(serializer),
            Self::Clear => serializer.serialize_none(),
        }
    }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for Override<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // `#[serde(default)]` handles a missing key; a present `null` maps to
        // `Clear`, any other value to `Set`.
        let value = Option::<serde_yaml_ng::Value>::deserialize(deserializer)?;
        match value {
            None => Ok(Self::Clear),
            Some(value) => serde_yaml_ng::from_value(value)
                .map(Override::Set)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// A single patch (mirrors `PatchOptions`).
#[derive(Clone, Debug, Deserialize, serde::Serialize, Default)]
pub struct PatchOptions {
    /// The target entry id; `''` is treated as absent.
    pub id: Option<String>,
    /// Entries inserted into the target group entry (or appended at the root
    /// when no `id` is given).
    #[serde(default)]
    pub insert: Option<Vec<EntryOptions>>,
    /// The expected name of the target entry; a mismatch warns and skips.
    pub name: Option<String>,
    /// `config` override; `null` clears the field.
    #[serde(default, skip_serializing_if = "Override::is_absent")]
    pub config: Override<serde_yaml_ng::Value>,
    /// `disabled` override; `null` clears the field.
    #[serde(default, skip_serializing_if = "Override::is_absent")]
    pub disabled: Override<bool>,
    /// `group` override; `null` clears the field.
    #[serde(default, skip_serializing_if = "Override::is_absent")]
    pub group: Override<bool>,
    /// `inject` override; `null` clears the field.
    #[serde(default, skip_serializing_if = "Override::is_absent")]
    pub inject: Override<Vec<String>>,
    /// `intercept` override; `null` clears the field.
    #[serde(default, skip_serializing_if = "Override::is_absent")]
    pub intercept: Override<serde_yaml_ng::Value>,
    /// `isolate` override; `null` clears the field.
    #[serde(default, skip_serializing_if = "Override::is_absent")]
    pub isolate: Override<HashMap<String, IsolateValue>>,
    /// Unknown patch keys are written to the target entry verbatim.
    #[serde(flatten, default)]
    pub extra: HashMap<String, serde_yaml_ng::Value>,
}

/// The include plugin: registers a plugin that mounts a file-backed tree.
pub fn include_plugin() -> Plugin {
    Plugin {
        name: Some("include".to_string()),
        inject: vec![("loader".to_string(), None)],
        is_group: false,
        apply: Arc::new(|ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
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
                        .lock()
                        .as_ref()
                        .is_some_and(|candidate| Arc::ptr_eq(candidate, &fiber))
                })
                .expect("include entry");
            let subgroup = loader.tree_handle().attach_subgroup(&entry);
            let loader = loader;
            Effect::Async(Box::pin(async move {
                mount_include(&loader, &subgroup, &config, &entry).await;
                Ok(sync_disposer(|| {}))
            }))
        }),
    }
}

async fn mount_include(
    loader: &Loader,
    subgroup: &Arc<EntryGroup>,
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

    // Tree writes are debounced to a single atomic file write per
    // event-loop turn, and `loader/config-update` fires on every write.
    let state = Arc::new(IncludeWriteState {
        filename,
        readonly: AtomicBool::new(false),
        pending: AtomicBool::new(false),
    });
    state
        .readonly
        .store(!check_writable(&state.filename), Ordering::Release);
    let loader_ctx = loader.ctx.clone();
    let tree = loader.tree_handle();
    let state_for_write = state;
    let subgroup_for_write = subgroup.clone();
    let tree_for_write = tree.clone();
    tree.write_callback.store(Arc::new(Some(Arc::new(move || {
        loader_ctx.emit("loader/config-update", &[]);
        if state_for_write.pending.load(Ordering::Acquire) {
            return;
        }
        state_for_write.pending.store(true, Ordering::Release);
        let state = state_for_write.clone();
        let subgroup = subgroup_for_write.clone();
        let tree = tree_for_write.clone();
        tokio::task::spawn(async move {
            // `yield_now` mirrors the TS `setTimeout(0)` debounce boundary:
            // writes issued in the same turn coalesce into one disk write.
            tokio::task::yield_now().await;
            // Re-resolve the current subgroup node: structural snapshots are
            // immutable, so the captured handle may be stale after reloads.
            let subgroup = tree.current_group(&subgroup);
            let _ = state.write_once(&subgroup);
            state.pending.store(false, Ordering::Release);
        });
    }))));
}

/// Refreshes the include entry whose config file is `filename` (config file
/// changes trigger a reload of the include tree instead of an HMR).
pub async fn refresh_include_file(loader: &Loader, filename: &Path) -> bool {
    let canonical = match fs::canonicalize(filename) {
        Ok(path) => path,
        Err(_) => filename.to_path_buf(),
    };
    for entry in loader.tree_handle().entries() {
        let Some(config) = entry.options.lock().config.clone() else {
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
        let Some(subgroup) = entry.subgroup() else {
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
    readonly: AtomicBool,
    pending: AtomicBool,
}

impl IncludeWriteState {
    fn write_once(&self, subgroup: &Arc<EntryGroup>) -> Result<(), String> {
        if self.readonly.load(Ordering::Acquire) {
            return Err("cannot overwrite readonly config".to_string());
        }
        let entries: Vec<EntryOptions> = subgroup
            .entries
            .iter()
            .map(|entry| entry.options.lock().clone())
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

/// A path into the entry tree: `path[0]` indexes `data`; deeper indices walk
/// into the enclosing group's `config` sequence (mirrors the recursive
/// `buildMap` in entry.ts).
type EntryPath = Vec<usize>;

/// Indexes every entry (recursively, including nested group children) by id.
fn index_entries(data: &[EntryOptions]) -> HashMap<String, EntryPath> {
    fn walk(
        entries: &[EntryOptions],
        prefix: &mut EntryPath,
        map: &mut HashMap<String, EntryPath>,
    ) {
        for (index, entry) in entries.iter().enumerate() {
            if !entry.id.is_empty() {
                let mut path = prefix.clone();
                path.push(index);
                map.insert(entry.id.clone(), path);
            }
            if entry.group == Some(true)
                && let Some(Value::Sequence(children)) = &entry.config
            {
                let children: Vec<EntryOptions> =
                    serde_yaml_ng::from_value(Value::Sequence(children.clone()))
                        .unwrap_or_default();
                prefix.push(index);
                walk(&children, prefix, map);
                prefix.pop();
            }
        }
    }
    let mut map = HashMap::new();
    walk(data, &mut Vec::new(), &mut map);
    map
}

/// Applies `f` to the entry at `path`, re-serializing each nested group's
/// `config` sequence along the way (owned recursion avoids borrow conflicts).
fn mutate_at_path(
    mut entries: Vec<EntryOptions>,
    path: &[usize],
    f: impl FnOnce(&mut EntryOptions),
) -> Vec<EntryOptions> {
    let Some((&head, rest)) = path.split_first() else {
        return entries;
    };
    let Some(entry) = entries.get_mut(head) else {
        return entries;
    };
    if rest.is_empty() {
        f(entry);
        return entries;
    }
    let children: Vec<EntryOptions> = entry
        .config
        .as_ref()
        .and_then(|value| serde_yaml_ng::from_value(value.clone()).ok())
        .unwrap_or_default();
    let updated = mutate_at_path(children, rest, f);
    entry.config = serde_yaml_ng::to_value(updated).ok();
    entries
}

/// Appends `insert` to the group at `path`, returning whether the target was
/// a group (mirrors `target.config.push(...insert)` with the `[]` fallback).
fn insert_into_group(
    entries: Vec<EntryOptions>,
    path: &[usize],
    insert: Vec<EntryOptions>,
) -> (Vec<EntryOptions>, bool) {
    let mut inserted = false;
    let entries = mutate_at_path(entries, path, |entry| {
        if entry.group != Some(true) {
            return;
        }
        let mut children: Vec<EntryOptions> = entry
            .config
            .as_ref()
            .and_then(|value| serde_yaml_ng::from_value(value.clone()).ok())
            .unwrap_or_default();
        children.extend(insert);
        entry.config = serde_yaml_ng::to_value(children).ok();
        inserted = true;
    });
    (entries, inserted)
}

fn apply_patches(
    mut data: Vec<EntryOptions>,
    patches: &[PatchOptions],
    ctx: &Context,
) -> Vec<EntryOptions> {
    if patches.is_empty() {
        return data;
    }
    // JS builds `entryMap` once before the loop: entries inserted by an
    // earlier patch are not addressable by later patches. Keep that behavior,
    // but re-index before each patch so index shifts never invalidate paths.
    let initial_ids: HashSet<String> = index_entries(&data).into_keys().collect();

    for patch in patches {
        // `id: ''` is treated as absent, matching the JS `if (id)` check.
        let id = patch.id.as_deref().filter(|id| !id.is_empty());
        let path = match id {
            Some(id) if initial_ids.contains(id) => index_entries(&data).remove(id),
            _ => None,
        };

        if let Some(insert) = &patch.insert {
            if let Some(id) = id {
                let Some(path) = path else {
                    ctx.logger()
                        .warn(format!("patch insert: entry {id} not found"));
                    continue;
                };
                let (updated, inserted) = insert_into_group(data, &path, insert.clone());
                data = updated;
                if inserted {
                    continue;
                }
                ctx.logger()
                    .warn(format!("patch insert: entry {id} is not a group"));
            } else {
                data.extend(insert.clone());
            }
            continue;
        }

        let Some(id) = id else {
            ctx.logger()
                .warn("patch: id is required for non-insert patches");
            continue;
        };
        let Some(path) = path else {
            ctx.logger().warn(format!("patch: entry {id} not found"));
            continue;
        };
        data = mutate_at_path(data, &path, |entry| {
            apply_overrides(entry, patch.name.as_deref(), patch, ctx, id);
        });
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
    if !overrides.config.is_absent() {
        entry.config = overrides.config.clone().into_option();
    }
    if !overrides.disabled.is_absent() {
        entry.disabled = overrides.disabled.clone().into_option();
    }
    if !overrides.group.is_absent() {
        entry.group = overrides.group.clone().into_option();
    }
    if !overrides.inject.is_absent() {
        entry.inject = overrides.inject.clone().into_option();
    }
    if !overrides.intercept.is_absent() {
        entry.intercept = overrides.intercept.clone().into_option();
    }
    if !overrides.isolate.is_absent() {
        entry.isolate = overrides.isolate.clone().into_option();
    }
    for (key, value) in &overrides.extra {
        entry.extra.insert(key.clone(), value.clone());
    }
}
