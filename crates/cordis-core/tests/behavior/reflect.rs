//! Ported cases from `packages/core/tests/reflect.spec.ts` (story card B10).

use std::rc::Rc;

use cordis_core::{Context, Effect, Plugin, Service};

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
                    apply: Rc::new(|ctx: &Context, _config| {
                        // Reserved properties (`prototype`, `constructor`) are
                        // ordinary Rust items; no special handling is needed.
                        let error = ctx.get_str_strict("bar").unwrap_err();
                        assert_eq!(error, "cannot get property \"bar\" without inject");
                        let error = ctx.set_str("bar", Rc::new(0i32)).unwrap_err();
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
                    apply: Rc::new(|ctx: &Context, _config| {
                        let error = ctx.set_str("foo", Rc::new(0i32)).unwrap_err();
                        assert_eq!(error, "cannot set property \"foo\" without provide");
                        drop(ctx.provide::<Foo>(Rc::new(Foo)).unwrap());
                        let error = ctx.provide::<Foo>(Rc::new(Foo)).unwrap_err();
                        assert!(
                            error.contains("service \"foo\" has been registered at <root>"),
                            "{error}"
                        );
                        ctx.set_str("foo", Rc::new(1i32)).unwrap();
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
            drop(root.provide::<Foo>(Rc::new(Foo)).unwrap());
            let fiber = root.inject(&["foo"], Rc::new(|_ctx, _config| Effect::None));
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
            drop(root.provide::<Foo>(Rc::new(Foo)).unwrap());
            root.mixin("foo", &[("bar", "foo.bar")]).unwrap();
            drop(root.provide_str("foo.bar", Rc::new(1i32)).unwrap());

            // foo is a service, bar is a mixin accessor.
            assert!(root.get_str("foo").is_some());
            assert!(root.resolve_assoc("foo", "bar").is_some());
            assert!(root.get_str("root").is_none());
        })
        .await;
}
