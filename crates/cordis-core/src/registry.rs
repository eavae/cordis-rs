//! Plugin registration and runtime management.
//!
//! Story card B2 provides the minimal registration machinery needed by the
//! fiber lifecycle; story card B4 formalizes the full plugin contract
//! (invalid-plugin errors, inject declaration forms, registry queries).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::context::{Context, ContextInner, InterceptLayer};
use crate::error::ConfigValidator;
use crate::fiber::{Epoch, Fiber, FiberState};
use crate::service::{ApplyFn, Effect, Service};

/// A plugin declaration (minimal form; B4 extends it).
#[derive(Clone)]
pub struct Plugin {
    /// Optional plugin name used for fiber naming.
    pub name: Option<String>,
    /// Declared inject dependencies: name → optional per-inject config.
    pub inject: Vec<(String, Option<Rc<dyn Any>>)>,
    /// The apply callback.
    pub apply: ApplyFn,
    /// Whether this plugin is a group container (mirrors the `EntryGroup.key`
    /// marker on the `Group` class in the TS loader).
    pub is_group: bool,
}

impl Plugin {
    /// The declared inject names.
    pub fn inject_names(&self) -> impl Iterator<Item = &str> {
        self.inject.iter().map(|(name, _)| name.as_str())
    }
}

/// A plugin runtime shared by all fibers of the same plugin.
pub struct Runtime {
    pub(crate) name: Option<String>,
    pub(crate) callback: ApplyFn,
    pub(crate) fibers: RefCell<Vec<Rc<Fiber>>>,
    pub(crate) registry: RefCell<Option<Weak<RegistryService>>>,
}

impl Runtime {
    /// Number of live fibers of this runtime.
    pub fn fiber_count(&self) -> usize {
        self.fibers.borrow().len()
    }
}

/// Registry service, available on every context as `ctx.registry`.
#[derive(Default)]
pub struct RegistryService {
    counter: Cell<u64>,
    runtimes: RefCell<HashMap<usize, Rc<Runtime>>>,
}

impl Service for RegistryService {
    const NAME: &'static str = "registry";
}

impl std::fmt::Debug for RegistryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryService")
            .field("size", &self.runtimes.borrow().len())
            .finish()
    }
}

impl RegistryService {
    /// Allocates the next fiber id.
    pub(crate) fn next_counter(&self) -> u64 {
        let next = self.counter.get() + 1;
        self.counter.set(next);
        next
    }

    /// Registers a plugin on `parent` and returns its fiber.
    ///
    /// The fiber is pushed into the runtime's fiber list and its injects are
    /// resolved synchronously; the apply callback runs through the reload
    /// state machine (see [`Fiber::wait`](crate::Fiber::wait)).
    pub fn plugin(
        &self,
        parent: &Context,
        plugin: &Plugin,
        config: Option<Rc<dyn Any>>,
    ) -> Rc<Fiber> {
        self.plugin_with_validator(parent, plugin, config, None)
    }

    /// Registers a plugin with an optional config validator.
    pub fn plugin_with_validator(
        &self,
        parent: &Context,
        plugin: &Plugin,
        config: Option<Rc<dyn Any>>,
        validator: Option<ConfigValidator>,
    ) -> Rc<Fiber> {
        parent
            .fiber()
            .assert_active()
            .expect("cannot create effect on inactive context");

        let callback = plugin.apply.clone();
        let key = Rc::as_ptr(&callback) as *const () as usize;
        let registry_rc = parent.get::<RegistryService>().expect("registry service");
        let mut runtimes = self.runtimes.borrow_mut();
        let runtime = runtimes
            .entry(key)
            .or_insert_with(|| {
                Rc::new(Runtime {
                    name: plugin.name.clone(),
                    callback: callback.clone(),
                    fibers: RefCell::new(Vec::new()),
                    registry: RefCell::new(Some(Rc::downgrade(&registry_rc))),
                })
            })
            .clone();
        drop(runtimes);

        let uid = self.next_counter();
        let child_inner = build_child_inner(parent, &plugin.inject);
        let validation_error = validator
            .as_ref()
            .and_then(|validate| config.as_ref().and_then(|config| validate(config).err()));
        let fiber = Rc::new(Fiber {
            uid: Cell::new(Some(uid)),
            ctx: child_inner,
            parent: Some(parent.clone()),
            config: RefCell::new(config),
            state: Cell::new(FiberState::Pending),
            inject: RefCell::new(
                plugin
                    .inject
                    .iter()
                    .map(|(name, config)| (name.clone(), config.clone()))
                    .collect(),
            ),
            runtime: RefCell::new(Some(runtime.clone())),
            error: RefCell::new(None),
            epoch: RefCell::new(Epoch::Inactive),
            resolved: RefCell::new(HashMap::new()),
            disposables: RefCell::new(Vec::new()),
            inertia: RefCell::new(Default::default()),
            dispose: RefCell::new(None),
            _hooks: RefCell::new(HashMap::new()),
            validator: RefCell::new(validator),
        });

        // Mirror `parent.fiber.effect(...)` in fiber.ts: the registration
        // effect runs synchronously, and its disposer unregisters the fiber.
        let fiber_for_effect = fiber.clone();
        let handle = parent
            .fiber()
            .effect(
                move || {
                    runtime.fibers.borrow_mut().push(fiber_for_effect.clone());
                    let _ = parent_events_emit(parent, &fiber_for_effect);
                    if let Some(error) = validation_error.clone() {
                        fiber_for_effect
                            .log_error(&format!("{error} at <{}>", fiber_for_effect.name()));
                        *fiber_for_effect.error.borrow_mut() =
                            Some(Box::new(crate::fiber::FiberError::new(error.to_string())));
                        *fiber_for_effect.epoch.borrow_mut() = Epoch::Inactive;
                        fiber_for_effect.state.set(FiberState::Failed);
                        let fiber = fiber_for_effect.clone();
                        return Effect::Disposer(Box::new(move || {
                            let fiber = fiber.clone();
                            Box::pin(async move {
                                unregister_dispose(fiber).await;
                                Ok(())
                            })
                        }));
                    }
                    for name in fiber_for_effect.inject.borrow().keys() {
                        fiber_for_effect.check_impl(name);
                    }
                    fiber_for_effect.refresh();
                    let fiber = fiber_for_effect.clone();
                    Effect::Disposer(Box::new(move || {
                        let fiber = fiber.clone();
                        Box::pin(async move {
                            unregister_dispose(fiber).await;
                            Ok(())
                        })
                    }))
                },
                "ctx.plugin()",
            )
            .expect("parent fiber must be active");
        *fiber.dispose.borrow_mut() = Some(handle);
        let _ = parent_events_emit(parent, &fiber);
        fiber
    }

    /// Resolves the runtime key of a plugin callback.
    fn runtime_key(callback: &ApplyFn) -> usize {
        Rc::as_ptr(callback) as *const () as usize
    }

    /// Whether a plugin is registered.
    pub fn has(&self, plugin: &Plugin) -> bool {
        self.runtimes
            .borrow()
            .contains_key(&Self::runtime_key(&plugin.apply))
    }

    /// Number of registered runtimes.
    pub fn size(&self) -> usize {
        self.runtimes.borrow().len()
    }

    /// The keys (callback addresses) of registered runtimes.
    pub fn keys(&self) -> Vec<usize> {
        self.runtimes.borrow().keys().copied().collect()
    }

    /// The registered runtimes.
    pub fn values(&self) -> Vec<Rc<Runtime>> {
        self.runtimes.borrow().values().cloned().collect()
    }

    /// Deletes a plugin runtime, disposing all of its fibers (mirrors
    /// `registry.delete`).
    pub fn delete(&self, plugin: &Plugin) {
        let runtime = {
            let runtimes = self.runtimes.borrow();
            runtimes.get(&Self::runtime_key(&plugin.apply)).cloned()
        };
        let Some(runtime) = runtime else {
            return;
        };
        let fibers = runtime.fibers.borrow().clone();
        for fiber in fibers {
            tokio::task::spawn_local(fiber.dispose());
        }
    }

    /// Re-checks every fiber that injects `name` and applies the same-label
    /// filter (mirrors `ReflectService.notify`).
    pub(crate) fn notify(&self, name: &str, provider: &Context) -> Vec<Rc<Fiber>> {
        let provider_label = provider.inner.isolate_label(name);
        let runtimes = self.runtimes.borrow();
        let mut affected = Vec::new();
        for runtime in runtimes.values() {
            for fiber in runtime.fibers.borrow().iter() {
                if !fiber.inject.borrow().contains_key(name) {
                    continue;
                }
                if provider_label != fiber.ctx.isolate_label(name) {
                    continue;
                }
                fiber.check_impl(name);
                fiber.refresh();
                affected.push(fiber.clone());
            }
        }
        affected
    }

    /// Notifies fibers whose isolate label for `name` matches `labels`.
    pub(crate) fn notify_with_labels(&self, name: &str, labels: &[crate::Label]) -> Vec<Rc<Fiber>> {
        let runtimes = self.runtimes.borrow();
        let mut affected = Vec::new();
        for runtime in runtimes.values() {
            for fiber in runtime.fibers.borrow().iter() {
                if !fiber.inject.borrow().contains_key(name) {
                    continue;
                }
                let label = fiber.ctx.isolate_label(name);
                if !labels
                    .iter()
                    .any(|candidate| label.as_ref() == Some(candidate))
                {
                    continue;
                }
                fiber.check_impl(name);
                fiber.refresh();
                affected.push(fiber.clone());
            }
        }
        affected
    }

    /// Removes a runtime once its last fiber is disposed.
    pub(crate) fn remove_runtime(&self, fiber: &Rc<Fiber>) {
        if let Some(runtime) = &*fiber.runtime.borrow() {
            let key = Rc::as_ptr(&runtime.callback) as *const () as usize;

            self.runtimes.borrow_mut().remove(&key);
        }
    }
}

fn build_child_inner(
    parent: &Context,
    inject: &[(String, Option<Rc<dyn Any>>)],
) -> Rc<ContextInner> {
    let intercept = if inject.is_empty() {
        parent.inner.intercept.clone()
    } else {
        let entries = inject
            .iter()
            .filter_map(|(name, config)| {
                let config = config.as_ref()?.clone();
                Some((name.clone(), config))
            })
            .collect();
        Rc::new(InterceptLayer {
            entries: RefCell::new(entries),
            parent: Some(parent.inner.intercept.clone()),
        })
    };
    Rc::new(ContextInner {
        isolate: parent.inner.isolate.clone(),
        intercept,
        store: parent.inner.store.clone(),
        meta: RefCell::new(parent.inner.meta.borrow().clone()),
        props: parent.inner.props.clone(),
    })
}

async fn unregister_dispose(fiber: Rc<Fiber>) {
    if fiber.uid.replace(None).is_none() {
        return;
    }
    if let Some(parent) = &fiber.parent {
        let _ = parent_events_emit(parent, &fiber);
    }
    if let Some(runtime) = &*fiber.runtime.borrow() {
        let mut fibers = runtime.fibers.borrow_mut();
        if let Some(position) = fibers.iter().position(|f| Rc::ptr_eq(f, &fiber)) {
            fibers.remove(position);
        }
        if fibers.is_empty()
            && let Some(registry) = runtime.registry.borrow().as_ref().and_then(Weak::upgrade)
        {
            registry.remove_runtime(&fiber);
        }
    }
    fiber.set_epoch(Epoch::Inactive);
    let _ = fiber.wait().await;
}

/// Emits the `internal/plugin` event for a fiber (mirrors the TS fiber
/// constructor/dispose emits).
fn parent_events_emit(parent: &Context, fiber: &Rc<Fiber>) -> Result<(), ()> {
    if let Some(events) = parent.get::<crate::EventsService>() {
        let fiber_any: Rc<dyn Any> = fiber.clone();
        events.emit(parent, "internal/plugin", &[fiber_any]);
    }
    Ok(())
}
