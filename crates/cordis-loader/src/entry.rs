//! Entry, EntryGroup and EntryTree.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use cordis_core::{Context, Fiber, Plugin};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::evaluator::{MinijinjaEvaluator, evaluate_config};

/// The tree's write callback (config write-back).
pub type WriteCallback = Arc<dyn Fn() + Send + Sync>;

/// A single entry's options (mirrors `EntryOptions` in entry.ts).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryOptions {
    /// Stable entry id.
    pub id: String,
    /// Plugin name (or `cordis:` builtin).
    pub name: String,
    /// Plugin config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_yaml_ng::Value>,
    /// Whether this entry is a group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<bool>,
    /// Whether this entry is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// Declared inject dependencies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject: Option<Vec<String>>,
    /// Per-service isolate scopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate: Option<std::collections::HashMap<String, IsolateValue>>,
    /// Per-service intercept overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intercept: Option<serde_yaml_ng::Value>,
    /// Extra keys preserved from the config file (mirrors the open
    /// `EntryOptions` object in entry.ts; include patches may write arbitrary
    /// keys here and they round-trip through write-back).
    #[serde(flatten, default)]
    pub extra: std::collections::HashMap<String, serde_yaml_ng::Value>,
}

/// An isolate declaration: `true` for a local realm, a string for a shared
/// label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IsolateValue {
    /// `true` creates a local realm (label derived from the entry id);
    /// `false` disables isolation.
    Flag(bool),
    /// A shared label.
    Label(String),
}

/// A group of entries (mirrors `EntryGroup` in group.ts).
pub struct EntryGroup {
    /// The owning tree.
    pub tree: Arc<EntryTree>,
    /// The group's context; entries inherit its isolate and intercept
    /// layers.
    pub ctx: Context,
    /// The parent group, if any (`None` for the root group).
    pub parent: Option<Arc<Self>>,
    /// Direct child entries of this group.
    pub entries: Mutex<Vec<Arc<Entry>>>,
    /// The group plugin's fiber, once initialized.
    pub fiber: Mutex<Option<Arc<Fiber>>>,
    /// The entry that owns this group (when the group belongs to an entry).
    pub entry: Mutex<Option<Arc<Entry>>>,
}

impl EntryGroup {
    /// Creates an empty group under `parent` (or the root group when
    /// `parent` is `None`).
    pub fn new(tree: Arc<EntryTree>, ctx: Context, parent: Option<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self {
            tree,
            ctx,
            parent,
            entries: Mutex::new(Vec::new()),
            fiber: Mutex::new(None),
            entry: Mutex::new(None),
        })
    }
}

impl std::fmt::Debug for EntryGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryGroup")
            .field("entries", &self.entries.lock().unwrap().len())
            .finish()
    }
}

/// The entry tree container (mirrors `EntryTree` in tree.ts).
pub struct EntryTree {
    /// The tree's context (the loader's own context).
    pub ctx: Context,
    /// Whether apply/reload logs are emitted.
    pub enable_logs: bool,
    /// Name → plugin table (builtins/mocks; `.so` loading is handled by the
    /// loader).
    pub plugins: Arc<ArcSwap<HashMap<String, Plugin>>>,
    /// `cordis:` builtin plugins.
    pub builtins: Arc<ArcSwap<HashMap<String, Plugin>>>,
    /// Called after every structural change (config write-back).
    pub write_callback: ArcSwap<Option<WriteCallback>>,
    /// The root group of the tree.
    pub root: Arc<Mutex<Option<Arc<EntryGroup>>>>,
    /// Number of pending entry tasks (used by the `await` intercept).
    pub tasks: Arc<AtomicUsize>,
    /// Notifies waiters when a pending entry task settles.
    pub tasks_notify: Arc<Notify>,
}

impl EntryTree {
    /// The id separator (mirrors `EntryTree.sep`).
    pub const SEP: &'static str = ":";

    /// Creates a tree with an empty root group.
    pub fn new(ctx: &Context) -> Arc<Self> {
        let tree = Arc::new(Self {
            ctx: ctx.clone(),
            enable_logs: true,
            plugins: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            builtins: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            write_callback: ArcSwap::from_pointee(None),
            root: Arc::new(Mutex::new(None)),
            tasks: Arc::new(AtomicUsize::new(0)),
            tasks_notify: Arc::new(Notify::new()),
        });
        let root = EntryGroup::new(tree.clone(), ctx.clone(), None);
        *tree.root.lock().unwrap() = Some(root);
        tree
    }

    /// Resolves a plugin by name. `cordis:` names hit builtins only; other
    /// names resolve through user-registered plugins first, with builtin
    /// aliases as a fallback (mirrors the TS loader, where `cordis:` hits
    /// builtins and package names are user-resolvable).
    pub fn import(&self, name: &str) -> Result<Plugin, String> {
        if let Some(builtin) = name.strip_prefix("cordis:") {
            return self
                .builtins
                .load_full()
                .get(builtin)
                .cloned()
                .ok_or_else(|| format!("cannot resolve builtin \"{name}\""));
        }
        if let Some(plugin) = self.plugins.load_full().get(name).cloned() {
            return Ok(plugin);
        }
        if let Some(plugin) = self.builtins.load_full().get(name).cloned() {
            return Ok(plugin);
        }
        Err(format!("cannot resolve plugin \"{name}\""))
    }

    /// Registers a plugin under a name (mock/builtin table).
    pub fn register_plugin(&self, name: &str, plugin: Plugin) {
        self.plugins.rcu(|table| {
            let mut next = (**table).clone();
            next.insert(name.to_string(), plugin.clone());
            Arc::new(next)
        });
    }

    /// Runs the write callback (mirrors `tree.write()`).
    pub fn write(&self) {
        // Load the callback as a lock-free snapshot: user callbacks may
        // re-enter the tree, and no lock is held while they run.
        let callback = self.write_callback.load_full();
        if let Some(callback) = callback.as_ref() {
            callback();
        }
    }

    /// The number of pending init tasks.
    pub fn get_tasks(&self) -> usize {
        let pending = self
            .entries()
            .iter()
            .filter(|entry| {
                entry.init_task.load(Ordering::Acquire)
                    || entry
                        .fiber
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|fiber| fiber.inertia_active())
                    || (entry.fiber.lock().unwrap().is_none()
                        && !entry.disabled()
                        && entry.options.lock().unwrap().group != Some(true))
            })
            .count();
        self.tasks.load(Ordering::Relaxed).max(pending)
    }

    /// All entries in the tree (depth-first).
    pub fn entries(&self) -> Vec<Arc<Entry>> {
        let mut result = Vec::new();
        let root = self.root.lock().unwrap();
        if let Some(root) = root.as_ref() {
            collect_entries(root, &mut result);
        }
        result
    }

    /// Finds an entry by id.
    pub fn resolve(&self, id: &str) -> Option<Arc<Entry>> {
        self.entries().into_iter().find(|entry| entry.id() == id)
    }

    /// Resolves an entry by id, walking nested groups via `:` separators.
    pub fn resolve_path(&self, id: &str) -> Result<Arc<Entry>, String> {
        let parts: Vec<&str> = id.split(Self::SEP).collect();
        let tree: &Self = self;
        let mut current: Arc<EntryGroup> = tree
            .root
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| format!("cannot resolve entry {id}"))?;
        for (index, part) in parts.iter().enumerate() {
            let is_last = index + 1 == parts.len();
            let entry = current
                .entries
                .lock()
                .unwrap()
                .iter()
                .find(|entry| entry.options.lock().unwrap().id == *part)
                .cloned()
                .ok_or_else(|| format!("cannot resolve entry {id}"))?;
            if is_last {
                return Ok(entry);
            }
            current = entry
                .subgroup
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| format!("cannot resolve entry {id}"))?;
        }
        Err(format!("cannot resolve entry {id}"))
    }

    /// Resolves a group; `None` resolves the root group.
    pub fn resolve_group(&self, id: Option<&str>) -> Result<Arc<EntryGroup>, String> {
        match id {
            None => self
                .root
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| "tree has no root".to_string()),
            Some(id) => {
                let entry = self.resolve_path(id)?;
                entry
                    .subgroup
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| format!("entry {id} is not a group"))
            }
        }
    }

    /// Generates a non-colliding 8-character hex id (mirrors `ensureId`).
    pub fn ensure_id(&self, options: &mut EntryOptions) -> String {
        if !options.id.is_empty() {
            return options.id.clone();
        }
        loop {
            let id = random_hex(8);
            if !self
                .entries()
                .iter()
                .any(|entry| entry.options.lock().unwrap().id == id)
            {
                options.id = id.clone();
                return id;
            }
        }
    }

    /// Creates an entry under a parent group (mirrors `tree.create`).
    pub fn create(
        self: &Arc<Self>,
        options: EntryOptions,
        parent: Option<&str>,
        position: usize,
    ) -> Arc<Entry> {
        let group = self
            .resolve_group(parent)
            .expect("cannot resolve parent group");
        let mut options = options;
        self.ensure_id(&mut options);
        let entry = Entry::new(self.clone(), group.clone(), options.clone());
        let mut entries = group.entries.lock().unwrap();
        let position = position.min(entries.len());
        entries.insert(position, entry.clone());
        drop(entries);
        self.write();
        entry.update(PartialEntryOptions::from_options(&options), true, false);
        entry.init_task.store(true, Ordering::Release);
        let this = entry.clone();
        tokio::task::spawn_local(async move {
            this.init_inner().await;
            this.init_task.store(false, Ordering::Release);
        });
        entry
    }

    /// Removes an entry (mirrors `tree.remove`).
    pub fn remove(self: &Arc<Self>, id: &str) {
        let entry = self
            .resolve_path(id)
            .unwrap_or_else(|error| panic!("{error}"));
        let parent = entry.parent.lock().unwrap();
        parent
            .entries
            .lock()
            .unwrap()
            .retain(|item| !Arc::ptr_eq(item, &entry));
        if let Some(fiber) = entry.fiber.lock().unwrap().clone() {
            self.spawn_dispose(fiber);
        }
        self.write();
    }

    /// Disposes a fiber as a counted tree task, so [`EntryTree::await_tree`]
    /// waits for it (mirrors `tree.await` observing `fiber.inertia`).
    pub(crate) fn spawn_dispose(self: &Arc<Self>, fiber: Arc<Fiber>) {
        self.tasks.fetch_add(1, Ordering::AcqRel);
        let tree = self.clone();
        tokio::task::spawn_local(async move {
            let _ = fiber.dispose().await;
            tree.tasks.fetch_sub(1, Ordering::AcqRel);
            tree.tasks_notify.notify_waiters();
        });
    }

    /// Moves an entry to another group (mirrors `tree.update(id, {}, parent)`).
    ///
    /// The entry's context is re-parented against the new group so isolate
    /// and intercept realms follow the move. The fiber is only restarted when
    /// a realm label actually changes; plain moves keep the fiber running
    /// (mirrors the TS patch-context flow).
    pub fn move_entry(self: &Arc<Self>, id: &str, parent: Option<&str>) {
        let entry = self
            .resolve_path(id)
            .unwrap_or_else(|error| panic!("{error}"));
        let source = entry.parent.lock().unwrap().clone();
        let target = self
            .resolve_group(parent)
            .unwrap_or_else(|error| panic!("{error}"));
        if Arc::ptr_eq(&source, &target) {
            return;
        }
        // Names whose realm might change with the move: the plugin's declared
        // injects (entry options may leave them to the plugin), plus the
        // service it provides under its own name.
        let mut names: Vec<String> = entry
            .options
            .lock()
            .unwrap()
            .inject
            .clone()
            .unwrap_or_default();
        if let Ok(plugin) = self.import(&entry.options.lock().unwrap().name) {
            for (name, _) in &plugin.inject {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        let own_name = entry.options.lock().unwrap().name.clone();
        if !names.contains(&own_name) {
            names.push(own_name);
        }
        let old_labels: HashMap<String, Option<cordis_core::Label>> = names
            .iter()
            .map(|name| (name.clone(), entry.ctx.isolate_label(name)))
            .collect();

        source
            .entries
            .lock()
            .unwrap()
            .retain(|item| !Arc::ptr_eq(item, &entry));
        target.entries.lock().unwrap().push(entry.clone());
        *entry.parent.lock().unwrap() = target;

        // Re-point the context chain at the new parent and re-apply the
        // entry's own overlay layers.
        entry.ctx.reparent(&entry.parent.lock().unwrap().ctx);
        entry.apply_overlay_layers();
        self.write();

        let changed: Vec<(
            String,
            Option<cordis_core::Label>,
            Option<cordis_core::Label>,
        )> = names
            .into_iter()
            .filter_map(|name| {
                let old = old_labels.get(&name).cloned().flatten();
                let new = entry.ctx.isolate_label(&name);
                (old != new).then_some((name, old, new))
            })
            .collect();

        if !changed.is_empty() {
            // The realm changed: restart the moved entry's fiber (clear it
            // first so the loader's self-dispose hook stays silent). Once the
            // entry re-provides under the new realm, wake dependents so their
            // inject checks re-run (mirrors the TS patch-context order).
            let fiber = entry.fiber.lock().unwrap().clone();
            *entry.fiber.lock().unwrap() = None;
            if let Some(fiber) = fiber {
                self.spawn_dispose(fiber);
            }
            if !entry.disabled() && entry.options.lock().unwrap().group != Some(true) {
                entry.init_task.store(true, Ordering::Release);
                let this = entry;
                let notify: Vec<(String, Vec<cordis_core::Label>)> = changed
                    .iter()
                    .map(|(name, old, new)| {
                        let mut labels = Vec::new();
                        if let Some(old) = old {
                            labels.push(old.clone());
                        }
                        if let Some(new) = new {
                            labels.push(new.clone());
                        }
                        (name.clone(), labels)
                    })
                    .collect();
                tokio::task::spawn_local(async move {
                    this.init_inner().await;
                    this.init_task.store(false, Ordering::Release);
                    for (name, labels) in notify {
                        let _ = this.ctx.notify_with_labels(&name, &labels);
                    }
                });
            }
        } else if entry.disabled() {
            let fiber = entry.fiber.lock().unwrap().clone();
            *entry.fiber.lock().unwrap() = None;
            if let Some(fiber) = fiber {
                self.spawn_dispose(fiber);
            }
        } else if entry.fiber.lock().unwrap().is_none()
            && entry.options.lock().unwrap().group != Some(true)
        {
            entry.init_task.store(true, Ordering::Release);
            let this = entry;
            tokio::task::spawn_local(async move {
                this.init_inner().await;
                this.init_task.store(false, Ordering::Release);
            });
        }
    }

    /// Updates an entry's options (mirrors `tree.update`).
    pub fn update_entry(&self, id: &str, options: PartialEntryOptions) {
        let entry = self
            .resolve_path(id)
            .unwrap_or_else(|error| panic!("{error}"));
        entry.update(options, false, false);
        if entry.fiber.lock().unwrap().is_none()
            && !entry.disabled()
            && entry.options.lock().unwrap().disabled != Some(true)
        {
            entry.init_task.store(true, Ordering::Release);
            let this = entry;
            tokio::task::spawn_local(async move {
                this.init_inner().await;
                this.init_task.store(false, Ordering::Release);
            });
        }
    }

    /// Awaits all pending entry tasks (mirrors `tree.await`).
    ///
    /// In-flight fiber cycles are awaited directly (event-driven, like the
    /// TS `entry.fiber.inertia` promise); init tasks settle through
    /// `init_inner`, which notifies `tasks_notify`.
    pub async fn await_tree(&self) {
        loop {
            let pending_fibers: Vec<Arc<Fiber>> = self
                .entries()
                .iter()
                .filter_map(|entry| {
                    entry
                        .fiber
                        .lock()
                        .unwrap()
                        .as_ref()
                        .filter(|fiber| fiber.inertia_active())
                        .cloned()
                })
                .collect();
            if pending_fibers.is_empty() && self.get_tasks() == 0 {
                return;
            }
            for fiber in pending_fibers {
                let _ = fiber.wait().await;
            }
            // Remaining pending entries are init tasks; wait for the next
            // completion notification, re-checking the condition so a
            // notification that fired before subscribing is not missed.
            while self.get_tasks() != 0 {
                let notified = self.tasks_notify.notified();
                if self.get_tasks() == 0 {
                    break;
                }
                notified.await;
            }
        }
    }
}

fn random_hex(length: usize) -> String {
    let mut result = String::with_capacity(length);
    for _ in 0..length {
        let digit = (fast_random() % 16) as u8;
        result.push(char::from_digit(digit as u32, 16).expect("hex digit"));
    }
    result
}

fn fast_random() -> u64 {
    use std::cell::Cell;
    use std::time::{SystemTime, UNIX_EPOCH};
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|state| {
        let seed = state.get();
        let next = if seed == 0 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64)
        } else {
            seed
        };
        // xorshift
        let mut x = next;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        state.set(x);
        x
    })
}

fn collect_entries(group: &Arc<EntryGroup>, result: &mut Vec<Arc<Entry>>) {
    for entry in group.entries.lock().unwrap().iter() {
        result.push(entry.clone());
        if let Some(subgroup) = &*entry.subgroup.lock().unwrap() {
            collect_entries(subgroup, result);
        }
    }
}

/// A single config entry (mirrors `Entry` in entry.ts).
pub struct Entry {
    /// The owning tree.
    pub tree: Arc<EntryTree>,
    /// The entry's context; re-parented when the entry moves between groups.
    /// The handle is immutable; overlay changes are atomic snapshot stores on
    /// the shared inner, so no lock is needed around it.
    pub ctx: Context,
    /// The owning group; updated when the entry moves between groups.
    pub parent: Mutex<Arc<EntryGroup>>,
    /// The entry's resolved options.
    pub options: Mutex<EntryOptions>,
    /// The entry's root fiber, once applied.
    pub fiber: Mutex<Option<Arc<Fiber>>>,
    /// The subgroup created when this entry is a group.
    pub subgroup: Mutex<Option<Arc<EntryGroup>>>,
    init_task: AtomicBool,
}

impl Entry {
    /// Creates an entry; call `update` immediately afterwards.
    pub fn new(tree: Arc<EntryTree>, parent: Arc<EntryGroup>, options: EntryOptions) -> Arc<Self> {
        let ctx = parent.ctx.clone();
        let ctx = ctx.with_isolate_layer().with_intercept_layer();
        let entry = Arc::new(Self {
            tree,
            ctx,
            parent: Mutex::new(parent),
            options: Mutex::new(options),
            fiber: Mutex::new(None),
            subgroup: Mutex::new(None),
            init_task: AtomicBool::new(false),
        });
        entry.apply_overlay_layers();
        entry
    }

    /// Merges this entry's `inject` declarations into `fiber`'s inject map
    /// (mirrors `Inject.resolve(entry.options.inject, fiber.inject)`; the
    /// entry's declaration wins on conflicts).
    pub(crate) fn merge_inject_into(&self, fiber: &Arc<Fiber>) {
        let Some(names) = self.options.lock().unwrap().inject.clone() else {
            return;
        };
        let mut inject = fiber.inject.lock().unwrap();
        for name in names {
            inject.insert(name, None);
        }
    }

    /// Rebuilds the entry's top overlay layer from its options.
    ///
    /// The isolate labels and intercept overrides are published together in a
    /// single atomic snapshot store ([`Context::apply_overlay`]), so readers
    /// never observe a half-applied overlay reconfiguration.
    fn apply_overlay_layers(&self) {
        let isolate = self
            .options
            .lock()
            .unwrap()
            .isolate
            .clone()
            .unwrap_or_default();
        let id = self.options.lock().unwrap().id.clone();
        let intercept = self
            .options
            .lock()
            .unwrap()
            .intercept
            .clone()
            .unwrap_or_default();
        let mut isolate_map: HashMap<String, cordis_core::Label> = HashMap::new();
        for (name, value) in isolate {
            let label = match value {
                IsolateValue::Flag(true) => Arc::<str>::from(format!("{name}#{id}")),
                IsolateValue::Label(label) => Arc::<str>::from(format!("{name}@{label}")),
                IsolateValue::Flag(false) => continue,
            };
            isolate_map.insert(name, label);
        }
        // The entry's own intercept overrides fill its top overlay layer;
        // parent layers stay reachable through the context chain (mirrors
        // the TS `entry.ctx[Context.intercept]` prototype chain).
        let mut intercept_map: HashMap<String, Arc<dyn Any + Send + Sync>> = HashMap::new();
        if let serde_yaml_ng::Value::Mapping(map) = intercept {
            for (name, value) in map {
                if let Some(name) = name.as_str() {
                    intercept_map.insert(name.to_string(), Arc::new(value));
                }
            }
        }
        self.ctx.apply_overlay(&isolate_map, &intercept_map);
    }

    /// The full id, prefixed by ancestor entry ids (mirrors `entry.id`).
    pub fn id(&self) -> String {
        let id = self.options.lock().unwrap().id.clone();
        match self.ancestor_entry() {
            Some(ancestor) => format!("{}{}{}", ancestor.id(), EntryTree::SEP, id),
            None => id,
        }
    }

    fn ancestor_entry(&self) -> Option<Arc<Self>> {
        self.parent.lock().unwrap().entry.lock().unwrap().clone()
    }

    /// Whether the entry (or an ancestor) is disabled (mirrors `entry.disabled`).
    pub fn disabled(&self) -> bool {
        if self.options.lock().unwrap().group == Some(true) {
            return false;
        }
        if self.options.lock().unwrap().disabled == Some(true) {
            return true;
        }
        let mut entry = self.ancestor_entry();
        while let Some(current) = entry {
            if current.options.lock().unwrap().disabled == Some(true) {
                return true;
            }
            entry = current.ancestor_entry();
        }
        false
    }

    /// Applies a partial options update (mirrors `entry.update`).
    pub fn update(
        self: &Arc<Self>,
        options: PartialEntryOptions,
        create: bool,
        clear_missing: bool,
    ) {
        let legacy = self.options.lock().unwrap().clone();
        {
            let mut current = self.options.lock().unwrap();
            if create {
                *current = options.into_options(current.id.clone(), current.name.clone());
            } else if clear_missing {
                options.apply_full(&mut current);
            } else {
                options.apply_to(&mut current);
            }
        }

        // Groups are always "enabled" per `disabled()`, but explicitly
        // disabling a group entry still stops its subtree.
        let group_disabled = self.options.lock().unwrap().group == Some(true)
            && self.options.lock().unwrap().disabled == Some(true);
        if group_disabled || self.disabled() {
            let fiber = self.fiber.lock().unwrap().clone();
            if let Some(fiber) = fiber {
                self.tree.spawn_dispose(fiber);
            }
            *self.fiber.lock().unwrap() = None;
            return;
        }

        let changed = self.options.lock().unwrap().diff(&legacy);
        if changed
            .iter()
            .any(|key| *key == "isolate" || *key == "intercept")
        {
            let old_isolate = legacy.isolate.clone().unwrap_or_default();
            let new_isolate = self
                .options
                .lock()
                .unwrap()
                .isolate
                .clone()
                .unwrap_or_default();
            let changed_names: Vec<String> = new_isolate
                .keys()
                .chain(old_isolate.keys())
                .filter(|name| new_isolate.get(*name) != old_isolate.get(*name))
                .cloned()
                .collect();
            let old_labels: HashMap<String, cordis_core::Label> = changed_names
                .iter()
                .map(|name| {
                    let label = self
                        .ctx
                        .isolate_label(name)
                        .unwrap_or_else(|| Arc::from("") as cordis_core::Label);
                    (name.clone(), label)
                })
                .collect();
            self.apply_overlay_layers();
            let fiber = self.fiber.lock().unwrap().clone();
            let is_group = self.options.lock().unwrap().group == Some(true);
            if is_group
                && let Some(fiber) = &fiber
                && fiber.uid().is_some()
            {
                let config = self.resolve_applied_config();
                let fiber = fiber.clone();
                tokio::task::spawn_local(fiber.update_with(config, true));
            } else if fiber.is_some() {
                // Migrate services provided by this entry's fiber to the new
                // labels (mirrors the loader's store migration; the provider
                // may be a child entry living under the changed realm).
                for (name, old_label) in &old_labels {
                    let new_label = self.ctx.isolate_label(name);
                    if let Some(new_label) = new_label {
                        self.ctx.migrate_label(name, old_label, &new_label);
                    }
                }
            }
            // Wake fibers that depend on the re-scoped names.
            for name in &changed_names {
                let mut labels = Vec::new();
                if let Some(old) = old_labels.get(name) {
                    labels.push(old.clone());
                }
                let new = self.ctx.isolate_label(name);
                if let Some(new) = new {
                    labels.push(new);
                }
                let _ = self.ctx.notify_with_labels(name, &labels);
            }
            return;
        }

        let has_fiber = self.fiber.lock().unwrap().is_some();
        if has_fiber {
            if changed.is_empty() {
                return;
            }

            self.patch_context();
        }
    }

    fn patch_context(&self) {
        // Clone out of the lock first: the config resolution below must not
        // run while the fiber guard is held (re-entry would self-deadlock).
        let fiber = self.fiber.lock().unwrap().clone();
        if let Some(fiber) = fiber {
            let config = self.resolve_applied_config();

            tokio::task::spawn_local(fiber.update_with(config, true));
        }
    }

    /// Resolves the config for the currently registered plugin (mirrors the
    /// TS `_resolveConfig(this.fiber.runtime!.callback)`), falling back to the
    /// raw config when the plugin cannot be re-imported.
    fn resolve_applied_config(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        let name = self.options.lock().unwrap().name.clone();

        match self.tree.import(&name) {
            Ok(plugin) => match self.resolve_config_value(&plugin) {
                Ok(config) => config,
                Err(error) => {
                    // Re-evaluation failed mid-lifecycle: keep the previous
                    // config and surface the error through the logger.
                    self.tree.ctx.logger().error(error);
                    self.raw_config()
                }
            },
            Err(_) => self.raw_config(),
        }
    }

    fn resolve_config_value(
        &self,
        plugin: &Plugin,
    ) -> Result<Option<Arc<dyn Any + Send + Sync>>, String> {
        // Group plugins receive their config list as-is (mirrors the TS
        // `_resolveConfig` check against `EntryGroup.key`).
        if plugin.is_group {
            return Ok(self.raw_config());
        }
        // Non-group plugins: evaluate `!expr` in the config (mirrors the TS
        // `_resolveConfig` interpolation).
        let Some(config) = self.options.lock().unwrap().config.clone() else {
            return Ok(None);
        };
        let Some(loader) = self.tree.ctx.get::<crate::Loader>() else {
            return Ok(Some(Arc::new(config) as Arc<dyn Any + Send + Sync>));
        };
        evaluate_config(&config, &MinijinjaEvaluator, &loader.eval_env())
            .map(|value| Some(Arc::new(value) as Arc<dyn Any + Send + Sync>))
            .map_err(|error| format!("config evaluation failed: {error}"))
    }

    fn raw_config(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        self.options
            .lock()
            .unwrap()
            .config
            .clone()
            .map(|value| Arc::new(value) as Arc<dyn Any + Send + Sync>)
    }

    /// Initializes the entry: imports the plugin and creates its fiber
    /// (idempotent, mirrors `entry.init`).
    pub async fn init(self: &Arc<Self>) {
        if self.init_task.swap(true, Ordering::AcqRel) {
            return;
        }
        self.init_inner().await;
        self.init_task.store(false, Ordering::Release);
    }

    /// Reloads the entry with the plugin currently registered under its
    /// name: disposes the old fiber and re-applies.
    pub async fn reload(self: &Arc<Self>) -> Result<(), String> {
        // Clear the fiber first so the loader's self-dispose hook does not
        // mistake this intentional reload for a plugin self-dispose.
        let fiber = self.fiber.lock().unwrap().clone();
        *self.fiber.lock().unwrap() = None;
        if let Some(fiber) = fiber {
            let _ = tokio::task::spawn_local(fiber.dispose()).await;
        }
        self.init_task.store(false, Ordering::Release);
        self.init().await;
        match self.fiber.lock().unwrap().clone() {
            Some(fiber) if fiber.state() == cordis_core::FiberState::Failed => {
                Err(format!("entry {} failed to reload", self.id()))
            }
            Some(_) => Ok(()),
            None => Err(format!("entry {} failed to reload", self.id())),
        }
    }

    /// Ungated initialization used by `create` (the pending flag is set by
    /// the caller).
    pub(crate) async fn init_inner(self: &Arc<Self>) {
        let result = self.import_and_apply().await;
        if let Err(error) = result {
            self.ctx.logger().error(error);
        }
        // The current init task is finishing; clear its flag so the settle
        // check below sees a quiet tree (mirrors the TS `entry.init`, which
        // notifies `loader` once all tasks settle).
        self.init_task.store(false, Ordering::Release);
        self.tree.tasks_notify.notify_waiters();
        if self.tree.get_tasks() == 0 {
            let _ = self.ctx.notify("loader");
        }
    }

    async fn import_and_apply(self: &Arc<Self>) -> Result<(), String> {
        if self.disabled() {
            return Ok(());
        }
        // The plugin is always resolved by `options.name` (mirrors the TS
        // `entry._init`, which imports `this.options.name`). `group: true`
        // only carries the lifecycle semantics of being a group container.
        let plugin = self.tree.import(&self.options.lock().unwrap().name)?;
        let config = self.resolve_config_value(&plugin)?;
        self.tree.tasks.fetch_add(1, Ordering::AcqRel);
        // Clone the context out of the lock before registering: the plugin
        // registration emits `internal/plugin`, whose loader callback
        // re-enters this entry's context (self-deadlock if the guard is held).
        let ctx = self.ctx.clone();
        let fiber = ctx.registry_plugin(&plugin, config);
        *self.fiber.lock().unwrap() = Some(fiber.clone());
        if let Some(loader) = ctx.get::<crate::Loader>() {
            loader.show_log("apply", self);
        }
        let result = fiber.wait().await.map_err(|error| error.to_string());
        self.tree.tasks.fetch_sub(1, Ordering::AcqRel);
        self.tree.tasks_notify.notify_waiters();
        result
    }

    /// The outer stack lines for error reporting (mirrors `getOuterStack`).
    pub fn get_outer_stack(&self) -> Vec<String> {
        let mut result = Vec::new();
        let mut entry: Option<Arc<Self>> = self.ancestor_entry();
        let mut own_id = self.options.lock().unwrap().id.clone();
        loop {
            let base = "cordis";
            result.push(format!("    at {base}#{own_id}"));
            match entry {
                Some(current) => {
                    own_id = current.options.lock().unwrap().id.clone();
                    entry = current.ancestor_entry();
                }
                None => break,
            }
        }
        result
    }
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("id", &self.id())
            .field("name", &self.options.lock().unwrap().name)
            .field("disabled", &self.disabled())
            .finish()
    }
}

/// Partial entry options for `update` (mirrors `Partial<EntryOptions>`).
#[derive(Clone, Debug, Default)]
pub struct PartialEntryOptions {
    /// Stable entry id.
    pub id: Option<String>,
    /// Plugin name (or `cordis:` builtin).
    pub name: Option<String>,
    /// Plugin config.
    pub config: Option<serde_yaml_ng::Value>,
    /// Whether this entry is a group.
    pub group: Option<bool>,
    /// Whether this entry is disabled.
    pub disabled: Option<bool>,
    /// Declared inject dependencies.
    pub inject: Option<Vec<String>>,
    /// Per-service isolate scopes.
    pub isolate: Option<std::collections::HashMap<String, IsolateValue>>,
    /// Per-service intercept overrides.
    pub intercept: Option<serde_yaml_ng::Value>,
    /// Extra keys preserved from the config file.
    pub extra: Option<std::collections::HashMap<String, serde_yaml_ng::Value>>,
}

impl PartialEntryOptions {
    /// Builds a full `EntryOptions` for creation.
    pub fn into_options(&self, id: String, name: String) -> EntryOptions {
        EntryOptions {
            id: self.id.clone().unwrap_or(id),
            name: self.name.clone().unwrap_or(name),
            config: self.config.clone(),
            group: self.group,
            disabled: self.disabled,
            inject: self.inject.clone(),
            isolate: self.isolate.clone(),
            intercept: self.intercept.clone(),
            extra: self.extra.clone().unwrap_or_default(),
        }
    }

    fn apply_to(&self, current: &mut EntryOptions) {
        if let Some(id) = &self.id {
            current.id = id.clone();
        }
        if let Some(name) = &self.name {
            current.name = name.clone();
        }
        if let Some(config) = &self.config {
            current.config = Some(config.clone());
        }
        if let Some(group) = self.group {
            current.group = Some(group);
        }
        if let Some(disabled) = self.disabled {
            current.disabled = Some(disabled);
        }
        if let Some(inject) = &self.inject {
            current.inject = Some(inject.clone());
        }
        if let Some(isolate) = &self.isolate {
            current.isolate = Some(isolate.clone());
        }
        if let Some(intercept) = &self.intercept {
            current.intercept = Some(intercept.clone());
        }
        if let Some(extra) = &self.extra {
            current.extra = extra.clone();
        }
    }

    /// Applies every field; `None` values clear the current value.
    fn apply_full(&self, current: &mut EntryOptions) {
        if let Some(id) = &self.id {
            current.id = id.clone();
        }
        if let Some(name) = &self.name {
            current.name = name.clone();
        }
        current.config = self.config.clone();
        current.group = self.group;
        current.disabled = self.disabled;
        current.inject = self.inject.clone();
        current.isolate = self.isolate.clone();
        current.intercept = self.intercept.clone();
        current.extra = self.extra.clone().unwrap_or_default();
    }

    /// Builds a partial update from a full options set (used by `read`).
    pub fn from_options(options: &EntryOptions) -> Self {
        Self {
            id: Some(options.id.clone()),
            name: Some(options.name.clone()),
            config: options.config.clone(),
            group: options.group,
            disabled: options.disabled,
            inject: options.inject.clone(),
            isolate: options.isolate.clone(),
            intercept: options.intercept.clone(),
            extra: Some(options.extra.clone()),
        }
    }
}

impl EntryOptions {
    /// Returns the keys whose values differ from `legacy`.
    fn diff(&self, legacy: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.config != legacy.config {
            changed.push("config");
        }
        if self.disabled != legacy.disabled {
            changed.push("disabled");
        }
        if self.inject != legacy.inject {
            changed.push("inject");
        }
        if self.name != legacy.name {
            changed.push("name");
        }
        if self.isolate != legacy.isolate {
            changed.push("isolate");
        }
        if self.intercept != legacy.intercept {
            changed.push("intercept");
        }
        changed
    }
}
