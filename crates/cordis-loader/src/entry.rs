//! Entry, EntryGroup and EntryTree (story cards C1/C2/C3).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use cordis_core::{Context, Fiber, Plugin};
use serde::{Deserialize, Serialize};

/// The tree's write callback (config write-back).
pub type WriteCallback = Rc<dyn Fn()>;

/// A single entry's options (mirrors `EntryOptions` in entry.ts).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    /// Per-service isolate scopes (story card C4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolate: Option<std::collections::HashMap<String, IsolateValue>>,
    /// Per-service intercept overrides (story card C4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intercept: Option<serde_yaml_ng::Value>,
}

/// An isolate declaration: `true` for a local realm, a string for a shared
/// label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IsolateValue {
    Flag(bool),
    Label(String),
}

impl EntryOptions {
    /// Sorts keys the same way as `sortKeys` in entry.ts: `id`/`name` first,
    /// `config` last, the rest alphabetically (YAML writer concern, C7).
    pub(crate) fn sort_keys(&mut self) {}
}

/// A group of entries (mirrors `EntryGroup` in group.ts).
pub struct EntryGroup {
    pub tree: Rc<EntryTree>,
    pub ctx: Context,
    pub parent: Option<Rc<EntryGroup>>,
    pub entries: RefCell<Vec<Rc<Entry>>>,
    pub fiber: RefCell<Option<Rc<Fiber>>>,
    /// The entry that owns this group (when the group belongs to an entry).
    pub entry: RefCell<Option<Rc<Entry>>>,
}

impl EntryGroup {
    pub fn new(tree: Rc<EntryTree>, ctx: Context, parent: Option<Rc<EntryGroup>>) -> Rc<Self> {
        Rc::new(EntryGroup {
            tree,
            ctx,
            parent,
            entries: RefCell::new(Vec::new()),
            fiber: RefCell::new(None),
            entry: RefCell::new(None),
        })
    }
}

impl std::fmt::Debug for EntryGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryGroup")
            .field("entries", &self.entries.borrow().len())
            .finish()
    }
}

/// The entry tree container (mirrors `EntryTree` in tree.ts).
pub struct EntryTree {
    pub ctx: Context,
    pub enable_logs: bool,
    /// Name → plugin table (builtins/mocks; `.so` loading arrives in E3).
    pub plugins: Rc<RefCell<HashMap<String, Plugin>>>,
    /// `cordis:` builtin plugins.
    pub builtins: Rc<RefCell<HashMap<String, Plugin>>>,
    /// Called after every structural change (config write-back).
    pub write_callback: Rc<RefCell<Option<WriteCallback>>>,
    pub root: Rc<RefCell<Option<Rc<EntryGroup>>>>,
    pub tasks: Rc<Cell<usize>>,
}

impl EntryTree {
    /// The id separator (mirrors `EntryTree.sep`).
    pub const SEP: &'static str = ":";

    /// Creates a tree with an empty root group.
    pub fn new(ctx: &Context) -> Rc<Self> {
        let tree = Rc::new(EntryTree {
            ctx: ctx.clone(),
            enable_logs: true,
            plugins: Rc::new(RefCell::new(HashMap::new())),
            builtins: Rc::new(RefCell::new(HashMap::new())),
            write_callback: Rc::new(RefCell::new(None)),
            root: Rc::new(RefCell::new(None)),
            tasks: Rc::new(Cell::new(0)),
        });
        let root = EntryGroup::new(tree.clone(), ctx.clone(), None);
        *tree.root.borrow_mut() = Some(root);
        tree
    }

    /// Resolves a plugin by name; `cordis:` names hit builtins.
    pub fn import(&self, name: &str) -> Result<Plugin, String> {
        if let Some(plugin) = self.builtins.borrow().get(name).cloned() {
            return Ok(plugin);
        }
        if let Some(builtin) = name.strip_prefix("cordis:") {
            return self
                .builtins
                .borrow()
                .get(builtin)
                .cloned()
                .ok_or_else(|| format!("cannot resolve builtin \"{name}\""));
        }
        self.plugins
            .borrow()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("cannot resolve plugin \"{name}\""))
    }

    /// Registers a plugin under a name (mock/builtin table).
    pub fn register_plugin(&self, name: &str, plugin: Plugin) {
        self.plugins.borrow_mut().insert(name.to_string(), plugin);
    }

    /// Runs the write callback (mirrors `tree.write()`).
    pub fn write(&self) {
        if let Some(callback) = &*self.write_callback.borrow() {
            callback();
        }
    }

    /// The number of pending init tasks.
    pub fn get_tasks(&self) -> usize {
        let pending = self
            .entries()
            .iter()
            .filter(|entry| {
                entry.init_task.get()
                    || entry
                        .fiber
                        .borrow()
                        .as_ref()
                        .map(|fiber| fiber.inertia_active())
                        .unwrap_or(false)
                    || (entry.fiber.borrow().is_none()
                        && !entry.disabled()
                        && entry.options.borrow().group != Some(true))
            })
            .count();
        (self.tasks.get()).max(pending)
    }

    /// All entries in the tree (depth-first).
    pub fn entries(&self) -> Vec<Rc<Entry>> {
        let mut result = Vec::new();
        let root = self.root.borrow();
        if let Some(root) = root.as_ref() {
            collect_entries(root, &mut result);
        }
        result
    }

    /// Finds an entry by id.
    pub fn resolve(&self, id: &str) -> Option<Rc<Entry>> {
        self.entries().into_iter().find(|entry| entry.id() == id)
    }

    /// Resolves an entry by id, walking nested groups via `:` separators.
    pub fn resolve_path(&self, id: &str) -> Result<Rc<Entry>, String> {
        let parts: Vec<&str> = id.split(Self::SEP).collect();
        let tree: &EntryTree = self;
        let mut current: Rc<EntryGroup> = tree
            .root
            .borrow()
            .clone()
            .ok_or_else(|| format!("cannot resolve entry {id}"))?;
        for (index, part) in parts.iter().enumerate() {
            let is_last = index + 1 == parts.len();
            let entry = current
                .entries
                .borrow()
                .iter()
                .find(|entry| entry.options.borrow().id == *part)
                .cloned()
                .ok_or_else(|| format!("cannot resolve entry {id}"))?;
            if is_last {
                return Ok(entry);
            }
            current = entry
                .subgroup
                .borrow()
                .clone()
                .ok_or_else(|| format!("cannot resolve entry {id}"))?;
        }
        Err(format!("cannot resolve entry {id}"))
    }

    /// Resolves a group; `None` resolves the root group.
    pub fn resolve_group(&self, id: Option<&str>) -> Result<Rc<EntryGroup>, String> {
        match id {
            None => self
                .root
                .borrow()
                .clone()
                .ok_or_else(|| "tree has no root".to_string()),
            Some(id) => {
                let entry = self.resolve_path(id)?;
                entry
                    .subgroup
                    .borrow()
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
                .any(|entry| entry.options.borrow().id == id)
            {
                options.id = id.clone();
                return id;
            }
        }
    }

    /// Creates an entry under a parent group (mirrors `tree.create`).
    pub fn create(
        self: &Rc<Self>,
        options: EntryOptions,
        parent: Option<&str>,
        position: usize,
    ) -> Rc<Entry> {
        let group = self
            .resolve_group(parent)
            .expect("cannot resolve parent group");
        let mut options = options;
        self.ensure_id(&mut options);
        let entry = Entry::new(self.clone(), group.clone(), options.clone());
        let mut entries = group.entries.borrow_mut();
        let position = position.min(entries.len());
        entries.insert(position, entry.clone());
        drop(entries);
        self.write();
        entry.update(PartialEntryOptions::from_options(&options), true, false);
        entry.init_task.set(true);
        let this = entry.clone();
        tokio::task::spawn_local(async move {
            this.init_inner().await;
            this.init_task.set(false);
        });
        entry
    }

    /// Removes an entry (mirrors `tree.remove`).
    pub fn remove(&self, id: &str) {
        let entry = self
            .resolve_path(id)
            .unwrap_or_else(|error| panic!("{error}"));
        entry
            .parent
            .entries
            .borrow_mut()
            .retain(|item| !Rc::ptr_eq(item, &entry));
        if let Some(fiber) = entry.fiber.borrow().clone() {
            tokio::task::spawn_local(fiber.dispose());
        }
        self.write();
    }

    /// Updates an entry's options (mirrors `tree.update`).
    pub fn update_entry(&self, id: &str, options: PartialEntryOptions) {
        let entry = self
            .resolve_path(id)
            .unwrap_or_else(|error| panic!("{error}"));
        entry.update(options, false, false);
        if entry.fiber.borrow().is_none()
            && !entry.disabled()
            && entry.options.borrow().disabled != Some(true)
        {
            entry.init_task.set(true);
            let this = entry.clone();
            tokio::task::spawn_local(async move {
                this.init_inner().await;
                this.init_task.set(false);
            });
        }
    }

    /// Awaits all pending entry tasks (mirrors `tree.await`).
    pub async fn await_tree(&self) {
        loop {
            tokio::task::yield_now().await;
            let tasks = self.get_tasks();
            if tasks == 0 {
                return;
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
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1)
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

fn collect_entries(group: &Rc<EntryGroup>, result: &mut Vec<Rc<Entry>>) {
    for entry in group.entries.borrow().iter() {
        result.push(entry.clone());
        if let Some(subgroup) = &*entry.subgroup.borrow() {
            collect_entries(subgroup, result);
        }
    }
}

/// A single config entry (mirrors `Entry` in entry.ts).
pub struct Entry {
    pub tree: Rc<EntryTree>,
    pub ctx: Context,
    pub parent: Rc<EntryGroup>,
    pub options: RefCell<EntryOptions>,
    pub fiber: RefCell<Option<Rc<Fiber>>>,
    pub subgroup: RefCell<Option<Rc<EntryGroup>>>,
    init_task: Cell<bool>,
}

impl Entry {
    /// Creates an entry; call `update` immediately afterwards.
    pub fn new(tree: Rc<EntryTree>, parent: Rc<EntryGroup>, options: EntryOptions) -> Rc<Self> {
        let ctx = parent.ctx.clone();
        let ctx = ctx.with_isolate_layer().with_intercept_layer();
        let entry = Rc::new(Entry {
            tree,
            ctx,
            parent,
            options: RefCell::new(options),
            fiber: RefCell::new(None),
            subgroup: RefCell::new(None),
            init_task: Cell::new(false),
        });
        entry.apply_realm_layers();
        entry
    }

    /// Rebuilds the entry's top isolate/intercept layers from its options.
    fn apply_realm_layers(&self) {
        self.ctx.clear_isolate_layer();
        self.ctx.clear_intercept_layer();
        let isolate = self.options.borrow().isolate.clone().unwrap_or_default();
        for (name, value) in isolate {
            let label = match value {
                IsolateValue::Flag(true) => {
                    Rc::<str>::from(format!("{name}#{}", self.options.borrow().id))
                }
                IsolateValue::Label(label) => Rc::<str>::from(format!("{name}@{label}")),
                IsolateValue::Flag(false) => continue,
            };
            self.ctx.set_isolate(&name, label);
        }
        // Intercept values are interpreted by typed configs (C6); the layer
        // is created here so later cards can fill it.
    }

    /// The full id, prefixed by ancestor entry ids (mirrors `entry.id`).
    pub fn id(&self) -> String {
        let id = self.options.borrow().id.clone();
        match self.ancestor_entry() {
            Some(ancestor) => format!("{}{}{}", ancestor.id(), EntryTree::SEP, id),
            None => id,
        }
    }

    fn ancestor_entry(&self) -> Option<Rc<Entry>> {
        self.parent.entry.borrow().clone()
    }

    /// Whether the entry (or an ancestor) is disabled (mirrors `entry.disabled`).
    pub fn disabled(&self) -> bool {
        if self.options.borrow().group == Some(true) {
            return false;
        }
        if self.options.borrow().disabled == Some(true) {
            return true;
        }
        let mut entry = self.ancestor_entry();
        while let Some(current) = entry {
            if current.options.borrow().disabled == Some(true) {
                return true;
            }
            entry = current.ancestor_entry();
        }
        false
    }

    /// Applies a partial options update (mirrors `entry.update`).
    pub fn update(
        self: &Rc<Self>,
        options: PartialEntryOptions,
        create: bool,
        clear_missing: bool,
    ) {
        let legacy = self.options.borrow().clone();
        {
            let mut current = self.options.borrow_mut();
            if create {
                *current = options
                    .clone()
                    .into_options(current.id.clone(), current.name.clone());
            } else if clear_missing {
                options.apply_full(&mut current);
            } else {
                options.apply_to(&mut current);
            }
            current.sort_keys();
        }

        // Groups are always "enabled" per `disabled()`, but explicitly
        // disabling a group entry still stops its subtree.
        let group_disabled = self.options.borrow().group == Some(true)
            && self.options.borrow().disabled == Some(true);
        if group_disabled || self.disabled() {
            let fiber = self.fiber.borrow().clone();
            if let Some(fiber) = fiber {
                tokio::task::spawn_local(fiber.dispose());
            }
            *self.fiber.borrow_mut() = None;
            return;
        }

        let changed = self.options.borrow().diff(&legacy);
        if changed
            .iter()
            .any(|key| *key == "isolate" || *key == "intercept")
        {
            let old_isolate = legacy.isolate.clone().unwrap_or_default();
            let new_isolate = self.options.borrow().isolate.clone().unwrap_or_default();
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
                        .unwrap_or_else(|| Rc::from("") as cordis_core::Label);
                    (name.clone(), label)
                })
                .collect();
            self.apply_realm_layers();
            let fiber = self.fiber.borrow().clone();
            let is_group = self.options.borrow().group == Some(true);
            if is_group
                && let Some(fiber) = &fiber
                && fiber.uid.get().is_some()
            {
                let config = self.resolve_config_value();
                let fiber = fiber.clone();
                tokio::task::spawn_local(fiber.update_with(config, true));
            } else if let Some(fiber) = &fiber {
                // Migrate services provided by this entry's fiber to the new
                // labels (mirrors the loader's store migration).
                for (name, old_label) in &old_labels {
                    if let Some(new_label) = self.ctx.isolate_label(name) {
                        self.ctx
                            .migrate_label_if(name, old_label, &new_label, fiber);
                    }
                }
            }
            // Wake fibers that depend on the re-scoped names.
            for name in &changed_names {
                let mut labels = Vec::new();
                if let Some(old) = old_labels.get(name) {
                    labels.push(old.clone());
                }
                if let Some(new) = self.ctx.isolate_label(name) {
                    labels.push(new);
                }
                let _ = self.ctx.notify_with_labels(name, &labels);
            }
            return;
        }

        if self.fiber.borrow().is_some() {
            if changed.is_empty() {
                return;
            }
            self.patch_context();
        }
    }

    fn patch_context(&self) {
        if let Some(fiber) = self.fiber.borrow().clone() {
            let config = self.resolve_config_value();
            tokio::task::spawn_local(fiber.update_with(config, true));
        }
    }

    fn resolve_config_value(&self) -> Option<Rc<dyn std::any::Any>> {
        self.options
            .borrow()
            .config
            .clone()
            .map(|value| Rc::new(value) as Rc<dyn std::any::Any>)
    }

    /// Initializes the entry: imports the plugin and creates its fiber
    /// (idempotent, mirrors `entry.init`).
    pub async fn init(self: &Rc<Self>) {
        if self.init_task.replace(true) {
            return;
        }
        self.init_inner().await;
        self.init_task.set(false);
    }

    /// Ungated initialization used by `create` (the pending flag is set by
    /// the caller).
    pub(crate) async fn init_inner(self: &Rc<Self>) {
        let result = self.import_and_apply().await;
        if let Err(error) = result {
            self.ctx.logger().error(error);
        }
        if self.tree.get_tasks() == 0 {
            let _ = self.ctx.notify("loader");
        }
    }

    async fn import_and_apply(self: &Rc<Self>) -> Result<(), String> {
        if self.disabled() {
            return Ok(());
        }
        let plugin = if self.options.borrow().group == Some(true) {
            self.tree.import("@cordisjs/plugin-group")?
        } else {
            self.tree.import(&self.options.borrow().name)?
        };
        self.tree.tasks.set(self.tree.tasks.get() + 1);
        let fiber = self
            .ctx
            .registry_plugin(&plugin, self.resolve_config_value());
        *self.fiber.borrow_mut() = Some(fiber.clone());
        if let Some(loader) = self.ctx.get::<crate::Loader>() {
            loader.show_log("apply", self);
        }
        let result = fiber.wait().await.map_err(|error| error.to_string());
        self.tree.tasks.set(self.tree.tasks.get().saturating_sub(1));
        result
    }

    /// The outer stack lines for error reporting (mirrors `getOuterStack`).
    pub fn get_outer_stack(&self) -> Vec<String> {
        let mut result = Vec::new();
        let mut entry: Option<Rc<Entry>> = self.ancestor_entry();
        let mut own_id = self.options.borrow().id.clone();
        loop {
            let base = "cordis";
            result.push(format!("    at {base}#{own_id}"));
            match entry {
                Some(current) => {
                    own_id = current.options.borrow().id.clone();
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
            .field("name", &self.options.borrow().name)
            .field("disabled", &self.disabled())
            .finish()
    }
}

/// Partial entry options for `update` (mirrors `Partial<EntryOptions>`).
#[derive(Clone, Debug, Default)]
pub struct PartialEntryOptions {
    pub id: Option<String>,
    pub name: Option<String>,
    pub config: Option<serde_yaml_ng::Value>,
    pub group: Option<bool>,
    pub disabled: Option<bool>,
    pub inject: Option<Vec<String>>,
    pub isolate: Option<std::collections::HashMap<String, IsolateValue>>,
    pub intercept: Option<serde_yaml_ng::Value>,
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
    }

    /// Builds a partial update from a full options set (used by `read`).
    pub fn from_options(options: &EntryOptions) -> Self {
        PartialEntryOptions {
            id: Some(options.id.clone()),
            name: Some(options.name.clone()),
            config: options.config.clone(),
            group: options.group,
            disabled: options.disabled,
            inject: options.inject.clone(),
            isolate: options.isolate.clone(),
            intercept: options.intercept.clone(),
        }
    }
}

impl EntryOptions {
    /// Returns the keys whose values differ from `legacy`.
    fn diff(&self, legacy: &EntryOptions) -> Vec<&'static str> {
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
