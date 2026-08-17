//! Ported cases from `packages/core/tests/invoke.spec.ts`.

use std::rc::Rc;

use cordis_core::{Config, Context, Effect, Plugin, Service, ShadowContext};

#[derive(Clone, Debug, PartialEq, Default)]
struct FooConfig {
    a: Option<i32>,
    b: Option<i32>,
    c: Option<i32>,
    d: Option<i32>,
}

impl Config for FooConfig {
    fn merge(&self, other: &Self) -> Self {
        FooConfig {
            a: other.a.or(self.a),
            b: other.b.or(self.b),
            c: other.c.or(self.c),
            d: other.d.or(self.d),
        }
    }
}

struct Foo {
    config: FooConfig,
}

impl Service for Foo {
    const NAME: &'static str = "foo";

    fn invoke(
        &self,
        ctx: &ShadowContext,
        init: Option<&Rc<dyn std::any::Any>>,
    ) -> Option<Rc<dyn std::any::Any>> {
        let init_config: Option<FooConfig> =
            init.and_then(|value| value.downcast_ref::<FooConfig>().cloned());
        let merged =
            ctx.resolve_config::<FooConfig>("foo", Some(&self.config), init_config.as_ref());
        Some(Rc::new(merged))
    }
}

impl Foo {
    fn extend_with(&self, config: FooConfig) -> Rc<Foo> {
        Rc::new(Foo {
            config: self.config.merge(&config),
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn functional_service() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|ctx: &Context, config: &Rc<dyn std::any::Any>| {
                        let config = config
                            .downcast_ref::<FooConfig>()
                            .cloned()
                            .unwrap_or_default();
                        drop(ctx.provide::<Foo>(Rc::new(Foo { config })).unwrap());
                        Effect::None
                    }),
                },
                Some(Rc::new(FooConfig {
                    a: Some(1),
                    ..Default::default()
                })),
            );
            fiber.wait().await.unwrap();

            // Access from context.
            let result = root
                .invoke::<Foo>(None)
                .expect("foo")
                .downcast_ref::<FooConfig>()
                .cloned()
                .unwrap();
            assert_eq!(
                result,
                FooConfig {
                    a: Some(1),
                    ..Default::default()
                }
            );

            let ctx1 = root.intercept(
                "foo",
                FooConfig {
                    b: Some(2),
                    ..Default::default()
                },
            );
            let result = ctx1
                .invoke::<Foo>(None)
                .expect("foo")
                .downcast_ref::<FooConfig>()
                .cloned()
                .unwrap();
            assert_eq!(
                result,
                FooConfig {
                    a: Some(1),
                    b: Some(2),
                    ..Default::default()
                }
            );
            let foo1 = ctx1.get::<Foo>().expect("foo");

            // Create an extension: the original instance is unchanged.
            let foo2 = root.get::<Foo>().expect("foo").extend_with(FooConfig {
                c: Some(3),
                ..Default::default()
            });
            let result = foo2
                .invoke(&ShadowContext::new(root.clone(), root.clone()), None)
                .expect("foo")
                .downcast_ref::<FooConfig>()
                .cloned()
                .unwrap();
            assert_eq!(
                result,
                FooConfig {
                    a: Some(1),
                    c: Some(3),
                    ..Default::default()
                }
            );

            let foo3 = foo1.extend_with(FooConfig {
                d: Some(4),
                ..Default::default()
            });
            let result = foo3
                .invoke(&ShadowContext::new(root.clone(), ctx1.clone()), None)
                .expect("foo")
                .downcast_ref::<FooConfig>()
                .cloned()
                .unwrap();
            assert_eq!(
                result,
                FooConfig {
                    a: Some(1),
                    b: Some(2),
                    d: Some(4),
                    ..Default::default()
                }
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn invoke_without_service_returns_none() {
    let root = Context::new();
    assert!(root.invoke::<Foo>(None).is_none());
}
