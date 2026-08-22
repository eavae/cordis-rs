//! Plugin registration and runtime management.
//!
//! The minimal registration machinery needed by the fiber lifecycle, plus
//! the full plugin contract (invalid-plugin errors, inject declaration
//! forms, registry queries).

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::context::{Context, ContextInner, OverlayLayer};
use crate::error::ConfigValidator;
use crate::events::EventFilter;
use crate::fiber::{Epoch, Fiber, FiberState};
use crate::service::{ApplyFn, Effect, Service};

/// The realm filter used by the `internal/service` broadcast: a listener
/// only receives the event when its own isolate label for the service name
/// matches the provider's (mirrors the temporary context with a filter in
/// `ReflectService.notify`).
struct ServiceRealmFilter {
    name: String,
    provider_label: Option<crate::Label>,
}

impl EventFilter for ServiceRealmFilter {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn filter(&self, hook_ctx: &Context) -> bool {
        hook_ctx.isolate_label(&self.name) == self.provider_label
    }
}

/// A plugin declaration (minimal form).
#[derive(Clone)]
pub struct Plugin {
    /// Optional plugin name used for fiber naming.
    pub name: Option<String>,
    /// Declared inject dependencies: name → optional per-inject config.
    pub inject: Vec<(String, Option<Arc<dyn Any + Send + Sync>>)>,
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
    pub(crate) fibers: Mutex<Vec<Arc<Fiber>>>,
    pub(crate) registry: Mutex<Option<Weak<RegistryService>>>,
}

impl Runtime {
    /// Number of live fibers of this runtime.
    pub fn fiber_count(&self) -> usize {
        self.fibers.lock().unwrap().len()
    }
}

/// Registry service, available on every context as `ctx.registry`.
///
/// # Lock ordering
///
/// The registry heads the snapshot lock group
/// `registry.runtimes → runtime.fibers → fiber.inject`. These locks may only
/// be nested while collecting the affected-fiber snapshot (see `notify` and
/// `notify_with_labels`); every per-fiber re-check runs afterwards with all
/// registry locks released, because `check_impl`/`refresh` can run user code
/// that re-enters the registry (e.g. `ctx.plugin()` or a nested
/// `ctx.notify()`). The per-fiber lifecycle locks
/// (`fiber.resolved → fiber.epoch → fiber.inertia`) form a separate group
/// and never nest with the registry group. Disposal follows the same rule:
/// the `fibers` guard is dropped before the runtime is removed from
/// `runtimes`, so the reverse edge `fibers → runtimes` never appears.
#[derive(Default)]
pub struct RegistryService {
    counter: AtomicU64,
    runtimes: Mutex<HashMap<usize, Arc<Runtime>>>,
}

impl Service for RegistryService {
    const NAME: &'static str = "registry";
}

impl std::fmt::Debug for RegistryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryService")
            .field("size", &self.runtimes.lock().unwrap().len())
            .finish()
    }
}

impl RegistryService {
    /// Allocates the next fiber id.
    pub(crate) fn next_counter(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
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
        config: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Arc<Fiber> {
        self.plugin_with_validator(parent, plugin, config, None)
    }

    /// Registers a plugin with an optional config validator.
    pub fn plugin_with_validator(
        &self,
        parent: &Context,
        plugin: &Plugin,
        config: Option<Arc<dyn Any + Send + Sync>>,
        validator: Option<ConfigValidator>,
    ) -> Arc<Fiber> {
        parent
            .fiber()
            .assert_active()
            .expect("cannot create effect on inactive context");

        let callback = plugin.apply.clone();
        let key = Arc::as_ptr(&callback) as *const () as usize;
        let registry_rc = parent.get::<Self>().expect("registry service");
        let mut runtimes = self.runtimes.lock().unwrap();
        let runtime = runtimes
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Runtime {
                    name: plugin.name.clone(),
                    callback: callback.clone(),
                    fibers: Mutex::new(Vec::new()),
                    registry: Mutex::new(Some(Arc::downgrade(&registry_rc))),
                })
            })
            .clone();
        drop(runtimes);

        let uid = self.next_counter();
        let child_inner = build_child_inner(parent, &plugin.inject);
        let validation_error = validator
            .as_ref()
            .and_then(|validate| config.as_ref().and_then(|config| validate(config).err()));
        let fiber = Arc::new(Fiber {
            uid: AtomicU64::new(uid),
            ctx: child_inner,
            parent: Some(parent.clone()),
            config: Mutex::new(config),
            state: AtomicU8::new(FiberState::Pending as u8),
            inject: Mutex::new(
                plugin
                    .inject
                    .iter()
                    .map(|(name, config)| (name.clone(), config.clone()))
                    .collect(),
            ),
            runtime: Some(runtime.clone()),
            error: Mutex::new(None),
            epoch: Mutex::new(Epoch::Inactive),
            resolved: Mutex::new(HashMap::new()),
            disposables: Mutex::new(Vec::new()),
            inertia: Mutex::new(crate::fiber::Inertia::default()),
            inertia_notify: Arc::new(tokio::sync::Notify::new()),
            dispose: Mutex::new(None),
            _hooks: Mutex::new(HashMap::new()),
            validator: Mutex::new(validator),
        });

        // Mirror `parent.fiber.effect(...)` in fiber.ts: the registration
        // effect runs synchronously, and its disposer unregisters the fiber.
        let fiber_for_effect = fiber.clone();
        let handle = parent
            .fiber()
            .effect(
                move || {
                    runtime
                        .fibers
                        .lock()
                        .unwrap()
                        .push(fiber_for_effect.clone());
                    let _ = parent_events_emit(parent, &fiber_for_effect);
                    if let Some(error) = validation_error.clone() {
                        fiber_for_effect
                            .log_error(&format!("{error} at <{}>", fiber_for_effect.name()));
                        *fiber_for_effect.error.lock().unwrap() =
                            Some(Box::new(crate::fiber::FiberError::new(error.to_string())));
                        *fiber_for_effect.epoch.lock().unwrap() = Epoch::Inactive;
                        fiber_for_effect.update_state(Some(FiberState::Failed));
                        let fiber = fiber_for_effect;
                        return Effect::Disposer(Box::new(move || {
                            let fiber = fiber;
                            Box::pin(async move {
                                unregister_dispose(fiber).await;
                                Ok(())
                            })
                        }));
                    }
                    // Snapshot the declared inject names before re-checking:
                    // `inject` belongs to the registry snapshot lock group
                    // and must not be held while `check_impl` touches
                    // `resolved` or runs user-provided check closures.
                    let inject_names: Vec<String> = fiber_for_effect
                        .inject
                        .lock()
                        .unwrap()
                        .keys()
                        .cloned()
                        .collect();
                    for name in &inject_names {
                        fiber_for_effect.check_impl(name);
                    }
                    fiber_for_effect.refresh();
                    let fiber = fiber_for_effect;
                    Effect::Disposer(Box::new(move || {
                        let fiber = fiber;
                        Box::pin(async move {
                            unregister_dispose(fiber).await;
                            Ok(())
                        })
                    }))
                },
                "ctx.plugin()",
            )
            .expect("parent fiber must be active");
        *fiber.dispose.lock().unwrap() = Some(handle);
        let _ = parent_events_emit(parent, &fiber);
        fiber
    }

    /// Resolves the runtime key of a plugin callback.
    fn runtime_key(callback: &ApplyFn) -> usize {
        Arc::as_ptr(callback) as *const () as usize
    }

    /// Whether a plugin is registered.
    pub fn has(&self, plugin: &Plugin) -> bool {
        self.runtimes
            .lock()
            .unwrap()
            .contains_key(&Self::runtime_key(&plugin.apply))
    }

    /// Number of registered runtimes.
    pub fn size(&self) -> usize {
        self.runtimes.lock().unwrap().len()
    }

    /// The keys (callback addresses) of registered runtimes.
    pub fn keys(&self) -> Vec<usize> {
        self.runtimes.lock().unwrap().keys().copied().collect()
    }

    /// The registered runtimes.
    pub fn values(&self) -> Vec<Arc<Runtime>> {
        self.runtimes.lock().unwrap().values().cloned().collect()
    }

    /// Deletes a plugin runtime, disposing all of its fibers (mirrors
    /// `registry.delete`).
    pub fn delete(&self, plugin: &Plugin) {
        let runtime = {
            let runtimes = self.runtimes.lock().unwrap();
            runtimes.get(&Self::runtime_key(&plugin.apply)).cloned()
        };
        let Some(runtime) = runtime else {
            return;
        };
        let fibers = runtime.fibers.lock().unwrap().clone();
        for fiber in fibers {
            tokio::task::spawn_local(fiber.dispose());
        }
    }

    /// Re-checks every fiber that injects `name` and applies the same-label
    /// filter (mirrors `ReflectService.notify`).
    ///
    /// Lock discipline: the registry and fiber-list locks only protect the
    /// affected-fiber snapshot below. The per-fiber re-checks run afterwards
    /// with every lock released, so a user-provided `check` closure that
    /// re-enters the registry (e.g. `ctx.plugin()` or a nested
    /// `ctx.notify()`) cannot self-deadlock on `runtimes`.
    pub(crate) fn notify(&self, name: &str, provider: &Context) -> Vec<Arc<Fiber>> {
        let provider_label = provider.inner.isolate_label(name);
        let affected = {
            let runtimes = self.runtimes.lock().unwrap();
            let mut affected = Vec::new();
            for runtime in runtimes.values() {
                let fibers = runtime.fibers.lock().unwrap();
                for fiber in fibers.iter() {
                    if !fiber.inject.lock().unwrap().contains_key(name) {
                        continue;
                    }
                    if provider_label != fiber.ctx.isolate_label(name) {
                        continue;
                    }
                    affected.push(fiber.clone());
                }
            }
            affected
        };
        for fiber in &affected {
            fiber.check_impl(name);
            fiber.refresh();
        }
        // `internal/service`: filter-directed broadcast on provide/remove
        // (mirrors `ReflectService.notify`). The payload is the current value
        // (or `()` when the service was just removed).
        let value: Arc<dyn Any + Send + Sync> = provider
            .inner
            .store
            .load_full()
            .by_label
            .get(&provider_label.clone().unwrap_or_default())
            .map_or_else(
                || Arc::new(()) as Arc<dyn Any + Send + Sync>,
                |entry| entry.value.clone(),
            );
        if let Some(events) = provider
            .inner
            .get_service_non_strict::<crate::EventsService>("events")
        {
            let filter = ServiceRealmFilter {
                name: name.to_string(),
                provider_label,
            };
            let args: Vec<Arc<dyn Any + Send + Sync>> = vec![Arc::new(name.to_string()), value];
            events.emit_with(provider, "internal/service", &args, Some(&filter));
        }
        affected
    }

    /// Notifies fibers whose isolate label for `name` matches `labels`.
    ///
    /// The affected-fiber snapshot is collected under the registry locks;
    /// the per-fiber re-checks (which may run user-provided `check` closures)
    /// execute after the locks are released (see [`Self::notify`]).
    pub(crate) fn notify_with_labels(
        &self,
        name: &str,
        labels: &[crate::Label],
    ) -> Vec<Arc<Fiber>> {
        let affected = {
            let runtimes = self.runtimes.lock().unwrap();
            let mut affected = Vec::new();
            for runtime in runtimes.values() {
                let fibers = runtime.fibers.lock().unwrap();
                for fiber in fibers.iter() {
                    if !fiber.inject.lock().unwrap().contains_key(name) {
                        continue;
                    }
                    let label = fiber.ctx.isolate_label(name);
                    if !labels
                        .iter()
                        .any(|candidate| label.as_ref() == Some(candidate))
                    {
                        continue;
                    }
                    affected.push(fiber.clone());
                }
            }
            affected
        };
        for fiber in &affected {
            fiber.check_impl(name);
            fiber.refresh();
        }
        affected
    }

    /// Removes a runtime by callback key once its last fiber is disposed.
    pub(crate) fn remove_runtime_by_key(&self, key: usize) {
        self.runtimes.lock().unwrap().remove(&key);
    }
}

fn build_child_inner(
    parent: &Context,
    inject: &[(String, Option<Arc<dyn Any + Send + Sync>>)],
) -> Arc<ContextInner> {
    let overlay = if inject.is_empty() {
        parent.inner.overlay.clone()
    } else {
        let entries = inject
            .iter()
            .filter_map(|(name, config)| {
                let config = config.as_ref()?.clone();
                Some((name.clone(), config))
            })
            .collect();
        OverlayLayer::with(HashMap::new(), entries, Some(parent.inner.overlay.clone()))
    };
    Arc::new(ContextInner {
        overlay,
        store: parent.inner.store.clone(),
        meta: Mutex::new(parent.inner.meta.lock().unwrap().clone()),
        props: parent.inner.props.clone(),
    })
}

async fn unregister_dispose(fiber: Arc<Fiber>) {
    if fiber.uid().is_none() {
        return;
    }
    fiber.set_uid(None);
    if let Some(parent) = &fiber.parent {
        let _ = parent_events_emit(parent, &fiber);
    }
    let remove = {
        let runtime = fiber.runtime.clone();
        let Some(runtime) = runtime else {
            return;
        };
        let mut fibers = runtime.fibers.lock().unwrap();
        if let Some(position) = fibers.iter().position(|f| Arc::ptr_eq(f, &fiber)) {
            fibers.remove(position);
        }
        let registry = runtime.registry.lock().unwrap().clone();
        let key = Arc::as_ptr(&runtime.callback) as *const () as usize;
        (fibers.is_empty(), registry, key)
    };
    if remove.0 {
        // The `fibers` and `registry` guards are scoped to the block above,
        // so the runtime removal below never touches them while they are
        // held.
        if let Some(registry) = remove.1.as_ref().and_then(Weak::upgrade) {
            registry.remove_runtime_by_key(remove.2);
        }
    }
    fiber.set_epoch(Epoch::Inactive);
    let _ = fiber.wait().await;
}

/// Emits the `internal/plugin` event for a fiber (mirrors the TS fiber
/// constructor/dispose emits).
fn parent_events_emit(parent: &Context, fiber: &Arc<Fiber>) -> Result<(), ()> {
    if let Some(events) = parent.get::<crate::EventsService>() {
        let fiber_any: Arc<dyn Any + Send + Sync> = fiber.clone();
        events.emit(parent, "internal/plugin", &[fiber_any]);
    }
    Ok(())
}
