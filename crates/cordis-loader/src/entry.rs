//! Entry, EntryGroup and EntryTree (story cards C1/C2/C3).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use cordis_core::{Context, Fiber, Plugin};
use serde::{Deserialize, Serialize};

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
}

impl EntryGroup {
    pub(crate) fn new(
        tree: Rc<EntryTree>,
        ctx: Context,
        parent: Option<Rc<EntryGroup>>,
    ) -> Rc<Self> {
        Rc::new(EntryGroup {
            tree,
            ctx,
            parent,
            entries: RefCell::new(Vec::new()),
            fiber: RefCell::new(None),
        })
    }
}

/// The entry tree container (mirrors `EntryTree` in tree.ts).
pub struct EntryTree {
    pub ctx: Context,
    pub enable_logs: bool,
    /// Name → plugin table (builtins/mocks; `.so` loading arrives in E3).
    pub plugins: RefCell<HashMap<String, Plugin>>,
    /// Called after every structural change (config write-back).
    pub write_callback: RefCell<Option<Rc<dyn Fn()>>>,
    pub root: RefCell<Option<Rc<EntryGroup>>>,
    pub tasks: RefCell<usize>,
}

impl EntryTree {
    /// The id separator (mirrors `EntryTree.sep`).
    pub const SEP: &'static str = ":";

    /// Creates a tree with an empty root group.
    pub fn new(ctx: &Context) -> Rc<Self> {
        let tree = Rc::new(EntryTree {
            ctx: ctx.clone(),
            enable_logs: true,
            plugins: RefCell::new(HashMap::new()),
            write_callback: RefCell::new(None),
            root: RefCell::new(None),
            tasks: RefCell::new(0),
        });
        let root = EntryGroup::new(tree.clone(), ctx.clone(), None);
        *tree.root.borrow_mut() = Some(root);
        tree
    }

    /// Resolves a plugin by name (builtins/mocks for now; `.so` in E3).
    pub fn import(&self, name: &str) -> Result<Plugin, String> {
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
        *self.tasks.borrow()
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
        Rc::new(Entry {
            tree,
            ctx: parent.ctx.clone(),
            parent,
            options: RefCell::new(options),
            fiber: RefCell::new(None),
            subgroup: RefCell::new(None),
            init_task: Cell::new(false),
        })
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
        self.parent.ctx.meta::<Entry>("entry")
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

        if self.disabled() {
            if let Some(fiber) = self.fiber.borrow().clone() {
                tokio::task::spawn_local(fiber.dispose());
            }
            return;
        }

        if self.fiber.borrow().is_some() {
            let changed = self.options.borrow().diff(&legacy);
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
        let result = self.import_and_apply().await;
        self.init_task.set(false);
        if let Err(error) = result {
            self.ctx.logger().error(error);
        }
        if self.tree.get_tasks() == 0 {
            // `reflect.notify(['loader'])` wakes loader injectors.
            let _ = self.ctx.notify("loader");
        }
    }

    async fn import_and_apply(self: &Rc<Self>) -> Result<(), String> {
        if self.disabled() {
            return Ok(());
        }
        let plugin = self.tree.import(&self.options.borrow().name)?;
        *self.tree.tasks.borrow_mut() += 1;
        let fiber = self
            .ctx
            .registry_plugin(&plugin, self.resolve_config_value());
        *self.fiber.borrow_mut() = Some(fiber.clone());
        let result = fiber.wait().await.map_err(|error| error.to_string());
        *self.tree.tasks.borrow_mut() -= 1;
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
        changed
    }
}
