//! Ported cases from `packages/core/tests/associate.spec.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use cordis_core::{Context, Effect, MixinAccessor, Plugin, Service, ShadowContext, service};

#[derive(Debug)]
struct Foo {
    qux: i32,
}

impl Service for Foo {
    const NAME: &'static str = "foo";
}

#[derive(Debug)]
struct FooBar;

impl Service for FooBar {
    const NAME: &'static str = "foo.bar";
}

#[tokio::test(flavor = "current_thread")]
async fn association_service_injection() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(ctx.provide::<Foo>(Arc::new(Foo { qux: 1 })).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            foo_fiber.wait().await.unwrap();
            let bar_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(ctx.provide::<FooBar>(Arc::new(FooBar)).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            bar_fiber.wait().await.unwrap();
            root.mixin("foo", &[("bar", "foo.bar")]).unwrap();

            assert_eq!(root.get::<Foo>().unwrap().qux, 1);
            let bar = root
                .resolve_assoc("foo", "bar")
                .expect("foo.bar must resolve")
                .downcast::<FooBar>()
                .ok();
            assert!(bar.is_some());

            bar_fiber.dispose().await;
            assert!(root.resolve_assoc("foo", "bar").is_none());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn association_property_injection() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            drop(root.provide_str("foo.bar", Arc::new(3i32)).unwrap());
            drop(
                root.provide_str("foo.baz", Arc::new("baz-value".to_string()))
                    .unwrap(),
            );
            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(ctx.provide::<Foo>(Arc::new(Foo { qux: 0 })).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            foo_fiber.wait().await.unwrap();
            root.mixin("foo", &[("bar", "foo.bar"), ("baz", "foo.baz")])
                .unwrap();

            // `foo.qux` is a plain field on the service.
            assert_eq!(root.get::<Foo>().unwrap().qux, 0);
            let bar = root
                .resolve_assoc("foo", "bar")
                .expect("bar")
                .downcast_ref::<i32>()
                .copied();
            assert_eq!(bar, Some(3));
            let baz = root
                .resolve_assoc("foo", "baz")
                .expect("baz")
                .downcast_ref::<String>()
                .cloned();
            assert_eq!(baz.as_deref(), Some("baz-value"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn association_duplicate_declaration_errors() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            drop(root.provide_str("foo.bar", Arc::new(1i32)).unwrap());
            root.mixin("foo", &[("bar", "foo.bar")]).unwrap();
            let error = root.mixin("foo", &[("bar", "foo.bar")]).unwrap_err();
            assert!(error.contains("already declared"), "{error}");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn association_mixin_get_set_forwards() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            // A service whose `secret` field is exposed through the mixin.
            let secret = Arc::new(AtomicI32::new(0));
            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: {
                        let secret = secret.clone();
                        Arc::new(move |ctx: &Context, _config| {
                            drop(
                                ctx.provide::<Foo>(Arc::new(Foo {
                                    qux: secret.load(Ordering::SeqCst),
                                }))
                                .unwrap(),
                            );
                            Effect::None
                        })
                    },
                },
                None,
            );
            foo_fiber.wait().await.unwrap();

            let secret_get = secret.clone();
            let secret_set = secret.clone();
            root.mixin_with(
                "foo",
                &[(
                    "secret",
                    MixinAccessor {
                        get: Arc::new(move |_ctx| {
                            Some(Arc::new(secret_get.load(Ordering::SeqCst)))
                        }),
                        set: Some(Arc::new(move |_ctx, value| {
                            secret_set.store(
                                value.downcast_ref::<i32>().copied().unwrap_or(0),
                                Ordering::SeqCst,
                            );
                        })),
                    },
                )],
            )
            .unwrap();

            assert_eq!(
                root.resolve_assoc("foo", "secret")
                    .unwrap()
                    .downcast_ref::<i32>()
                    .copied(),
                Some(0)
            );
            root.set_assoc("foo", "secret", Arc::new(42i32)).unwrap();
            assert_eq!(secret.load(Ordering::SeqCst), 42);
            assert_eq!(
                root.resolve_assoc("foo", "secret")
                    .unwrap()
                    .downcast_ref::<i32>()
                    .copied(),
                Some(42)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn association_get_set_requires_accessor() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let error = root
                .set_assoc("foo", "missing", Arc::new(1i32))
                .unwrap_err();
            assert!(
                error.contains("cannot set property \"foo.missing\" without provide"),
                "{error}"
            );
        })
        .await;
}

/// Mirrors the TS associate.spec.ts "inspect" regression (cordis issue #14):
/// a value passed through the service call chain keeps its type identity,
/// inspectable via `Debug` — the Rust counterpart of `arg.toString()` in JS.
#[derive(Debug)]
struct Widget;

#[service]
struct Inspector;

#[service]
impl Inspector {
    pub fn bar(&self, ctx: &ShadowContext, arg: &dyn std::fmt::Debug) -> String {
        let debug = format!("{arg:?}");
        assert!(debug.contains("Widget"), "{debug}");
        // Forward through another traced service hop; the value is opaque
        // (a trait object) yet its type name still shows through.
        ctx.inspector().expect("inspector").baz(arg)
    }

    pub fn baz(&self, _ctx: &ShadowContext, arg: &dyn std::fmt::Debug) -> String {
        format!("{arg:?}")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_preserves_type_identity() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::associate::InspectorServiceExt;

            let root = Context::new();
            drop(root.provide::<Inspector>(Arc::new(Inspector)).unwrap());

            let debug = root.inspector().expect("inspector").bar(&Widget);
            assert_eq!(debug, "Widget");
        })
        .await;
}
