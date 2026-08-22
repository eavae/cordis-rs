//! Ported cases from `packages/core/tests/reflect.spec.ts`, plus the
//! dynamic-access completion (`set` ownership, `get` three states, `has`,
//! `accessor`).

use parking_lot::Mutex;
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use cordis_core::{Context, Effect, FiberState, Plugin, ReflectService, Service, event_callback};

#[derive(Debug)]
struct Foo;

impl Service for Foo {
    const NAME: &'static str = "foo";
}

#[tokio::test(flavor = "current_thread")]
async fn context_is() {
    let root = Context::new();
    assert!(Context::is_context(&root));
    assert!(!Context::is_context(&5));
}

#[tokio::test(flavor = "current_thread")]
async fn access_check() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        // Reserved properties (`prototype`, `constructor`) are
                        // ordinary Rust items; no special handling is needed.
                        let error = ctx.get_str_strict("bar").unwrap_err();
                        assert_eq!(error, "cannot get property \"bar\" without inject");
                        let error = ctx.set_str("bar", Arc::new(0i32)).unwrap_err();
                        assert_eq!(error, "cannot set property \"bar\" without provide");
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();

            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        let error = ctx.set_str("foo", Arc::new(0i32)).unwrap_err();
                        assert_eq!(error, "cannot set property \"foo\" without provide");
                        drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                        let error = ctx.provide::<Foo>(Arc::new(Foo)).unwrap_err();
                        assert!(
                            error.contains("service \"foo\" has been registered at <root>"),
                            "{error}"
                        );
                        ctx.set_str("foo", Arc::new(1i32)).unwrap();
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_inject_leak() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            drop(root.provide::<Foo>(Arc::new(Foo)).unwrap());
            let fiber = root.inject(&["foo"], Arc::new(|_ctx, _config| Effect::None));
            fiber.wait().await.unwrap();
            assert!(fiber.context().get_str_strict("foo").is_ok());

            fiber.dispose().await;
            let error = fiber.context().get_str_strict("foo").unwrap_err();
            assert_eq!(
                error,
                "cannot get required service \"foo\" in inactive context"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_injection_and_mixin_get() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            drop(root.provide::<Foo>(Arc::new(Foo)).unwrap());
            root.mixin("foo", &[("bar", "foo.bar")]).unwrap();
            drop(root.provide_str("foo.bar", Arc::new(1i32)).unwrap());

            // foo is a service, bar is a mixin accessor.
            assert!(root.get_str("foo").is_some());
            assert!(root.resolve_assoc("foo", "bar").is_some());
            assert!(root.get_str("root").is_none());
        })
        .await;
}

/// `set` enforces ownership — only the providing fiber may update the
/// value, and injectors are notified after the update.
#[tokio::test(flavor = "current_thread")]
async fn reflect_set_ownership_check() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let service_updates = Arc::new(Mutex::new(Vec::<String>::new()));
            let updates = service_updates.clone();
            drop(
                root.on(
                    "internal/service",
                    event_callback(move |args: &[Arc<dyn Any + Send + Sync>]| {
                        let name = args[0].downcast_ref::<String>().unwrap().clone();
                        let value = args[1].downcast_ref::<i32>().copied().unwrap_or_default();
                        updates.lock().push(format!("{name}={value}"));
                        Ok(None)
                    }),
                    cordis_core::EventOptions::default(),
                )
                .unwrap(),
            );
            let provider = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(ctx.provide_str("foo", Arc::new(1i32)).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            provider.wait().await.unwrap();

            // A different fiber (root) cannot set the value.
            let error = root.set_str("foo", Arc::new(2i32)).unwrap_err();
            assert_eq!(
                error, "cannot set property \"foo\" in multiple fibers",
                "ownership must reject cross-fiber writes"
            );

            // The owning fiber can update, and the change is broadcast
            // through `internal/service` (the notify path).
            provider.context().set_str("foo", Arc::new(2i32)).unwrap();
            assert_eq!(
                root.get_str("foo").unwrap().downcast_ref::<i32>().copied(),
                Some(2),
                "owning fiber must update the value"
            );
            assert!(
                service_updates.lock().contains(&"foo=2".to_string()),
                "set must notify through internal/service: {:?}",
                service_updates.lock()
            );
        })
        .await;
}

/// `get(name, strict)` three states — registered+ACTIVE resolves for both
/// modes; missing resolves for neither; registered+non-ACTIVE resolves only
/// in non-strict mode.
#[tokio::test(flavor = "current_thread")]
async fn reflect_get_three_states() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            drop(root.provide_str("foo", Arc::new(1i32)).unwrap());

            // Registered + ACTIVE.
            assert_eq!(
                root.get_str("foo").unwrap().downcast_ref::<i32>().copied(),
                Some(1)
            );
            assert_eq!(
                root.get_str_non_strict("foo")
                    .unwrap()
                    .downcast_ref::<i32>()
                    .copied(),
                Some(1)
            );

            // Missing.
            assert!(root.get_str("nope").is_none());
            assert!(root.get_str_non_strict("nope").is_none());

            // A plugin whose disposal spans several polls keeps the root in a
            // non-ACTIVE state long enough to observe the difference.
            let plugin = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|_ctx: &Context, _config| {
                        Effect::Disposer(cordis_core::async_disposer(move || async move {
                            tokio::task::yield_now().await;
                            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
                        }))
                    }),
                },
                None,
            );
            plugin.wait().await.unwrap();

            // Registered + non-ACTIVE: the framework services survive a root
            // restart, so while the root fiber cycles through non-ACTIVE
            // states strict lookup is unavailable but non-strict still
            // resolves.
            let dispose = tokio::task::spawn_local(root.fiber().dispose());
            let mut saw_inactive = false;
            for _ in 0..2000 {
                tokio::task::yield_now().await;
                let strict = root.get_str("events");
                let non_strict = root.get_str_non_strict("events");
                if root.fiber().state() != FiberState::Active {
                    saw_inactive = true;
                    assert!(
                        strict.is_none(),
                        "strict get must be unavailable while the provider is inactive"
                    );
                    assert!(
                        non_strict.is_some(),
                        "non-strict get must keep resolving during unload"
                    );
                }
                if root.fiber().state() == FiberState::Active && strict.is_some() {
                    break;
                }
            }
            assert!(
                saw_inactive,
                "root restart must pass through a non-ACTIVE phase"
            );
            let _ = dispose.await;
        })
        .await;
}

/// `has` is true for registered services and accessors, false otherwise.
#[test]
fn reflect_has_sources() {
    let root = Context::new();
    assert!(!root.has_str("foo"), "no property yet");

    drop(root.provide_str("foo", Arc::new(1i32)).unwrap());
    assert!(root.has_str("foo"), "registered service counts");

    drop(
        root.accessor("bar", Arc::new(|_ctx| Some(Arc::new(2i32))), None)
            .unwrap(),
    );
    assert!(root.has_str("bar"), "registered accessor counts");
    assert!(!root.has_str("nope"));
}

/// `accessor(name, { get, set })` forwards reads/writes, rejects conflicts
/// with same-name services, and is removed when its fiber unloads.
#[tokio::test(flavor = "current_thread")]
async fn reflect_accessor_effect_governance() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let cell = Arc::new(AtomicI32::new(10));
            let get_cell = cell.clone();
            let set_cell = cell.clone();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(move |ctx: &Context, _config| {
                        let get_cell = get_cell.clone();
                        let set_cell = set_cell.clone();
                        drop(
                            ctx.accessor(
                                "secret",
                                Arc::new(move |_ctx| {
                                    Some(Arc::new(get_cell.load(Ordering::SeqCst)))
                                }),
                                Some(Arc::new(move |_ctx, value| {
                                    set_cell.store(
                                        value.downcast_ref::<i32>().copied().unwrap_or(0),
                                        Ordering::SeqCst,
                                    );
                                })),
                            )
                            .unwrap(),
                        );
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();

            // Reads and writes are forwarded to the closures.
            assert_eq!(
                root.resolve_assoc("x", "secret")
                    .unwrap()
                    .downcast_ref::<i32>()
                    .copied(),
                Some(10)
            );
            root.set_assoc("x", "secret", Arc::new(42i32)).unwrap();
            assert_eq!(cell.load(Ordering::SeqCst), 42);

            // Registering an accessor over a same-name service is rejected.
            drop(root.provide_str("taken", Arc::new(1i32)).unwrap());
            let error = root
                .accessor("taken", Arc::new(|_ctx| None), None)
                .unwrap_err();
            assert!(error.contains("already declared"), "{error}");

            // Disposing the fiber removes the accessor.
            let _ = tokio::task::spawn_local(fiber.dispose()).await;
            assert!(
                root.resolve_assoc("x", "secret").is_none(),
                "accessor must be removed with its fiber"
            );
        })
        .await;
}

/// The `ReflectService` facade exposes the same surface with explicit
/// context passing.
#[test]
fn reflect_service_facade() {
    let root = Context::new();
    let reflect = root.get::<ReflectService>().unwrap();

    drop(root.provide_str("foo", Arc::new(1i32)).unwrap());
    assert_eq!(
        reflect
            .get(&root, "foo", true)
            .unwrap()
            .downcast_ref::<i32>()
            .copied(),
        Some(1)
    );
    assert!(reflect.has(&root, "foo"));
    reflect.set(&root, "foo", Arc::new(2i32)).unwrap();
    assert_eq!(
        reflect
            .get(&root, "foo", false)
            .unwrap()
            .downcast_ref::<i32>()
            .copied(),
        Some(2)
    );
    assert!(reflect.get(&root, "nope", false).is_none());

    drop(
        reflect
            .accessor(&root, "bar", Arc::new(|_ctx| Some(Arc::new(3i32))), None)
            .unwrap(),
    );
    assert!(reflect.has(&root, "bar"));
}
