//! The Loader service (story cards C1/C6).

use std::ops::Deref;
use std::rc::Rc;

use cordis_core::{AnyNext, ApplyFn, Context, EventOptions, Fiber, Plugin, Service};

use crate::entry::{Entry, EntryGroup, EntryOptions, EntryTree, PartialEntryOptions};

/// The plugin loader service (mirrors `Loader` in loader/index.ts).
pub struct Loader {
    pub ctx: Context,
    pub tree: Rc<EntryTree>,
    pub name: &'static str,
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
        let tree = EntryTree::new(ctx);
        let loader = Rc::new(Loader {
            ctx: ctx.clone(),
            tree,
            name: "loader",
        });

        drop(ctx.provide::<Loader>(loader.clone()).unwrap());
        loader.register_internal_hooks();
        loader
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

    /// Reads a config list and reconciles the tree (mirrors `tree.read`).
    pub async fn read(&self, configs: Vec<EntryOptions>) {
        let root = self.tree.root.borrow().clone().expect("root");
        self.read_group(&root, configs).await;
    }

    async fn read_group(&self, group: &Rc<EntryGroup>, configs: Vec<EntryOptions>) {
        let mut next_entries: Vec<Rc<Entry>> = Vec::new();
        for options in configs {
            if let Some(existing) = group
                .entries
                .borrow()
                .iter()
                .find(|entry| entry.options.borrow().id == options.id)
                .cloned()
            {
                existing.update(PartialEntryOptions::from_options(&options), false, true);
                next_entries.push(existing);
            } else {
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

    /// Registers a mock plugin under a name (test helper, mirrors `mock`).
    pub fn mock(&self, name: &str, apply: ApplyFn) -> String {
        self.tree.plugins.borrow_mut().insert(
            name.to_string(),
            Plugin {
                name: None,
                inject: Vec::new(),
                apply,
            },
        );
        name.to_string()
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
}
