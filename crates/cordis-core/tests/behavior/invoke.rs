//! Ported cases from `packages/core/tests/invoke.spec.ts`.

use std::any::Any;
use std::sync::Arc;

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
        Self {
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
        init: Option<&Arc<dyn Any + Send + Sync>>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        let init_config: Option<FooConfig> =
            init.and_then(|value| value.downcast_ref::<FooConfig>().cloned());
        let merged =
            ctx.resolve_config::<FooConfig>("foo", Some(&self.config), init_config.as_ref());
        Some(Arc::new(merged))
    }
}

impl Foo {
    fn extend_with(&self, config: FooConfig) -> Arc<Self> {
        Arc::new(Self {
            config: self.config.merge(&config),
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn functional_service() {
    async {
        let root = Context::new();
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: Arc::new(|ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                    let config = config
                        .downcast_ref::<FooConfig>()
                        .cloned()
                        .unwrap_or_default();
                    drop(ctx.provide::<Foo>(Arc::new(Foo { config })).unwrap());
                    Effect::None
                }),
            },
            Some(Arc::new(FooConfig {
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
            .invoke(&ShadowContext::new(root.clone(), ctx1), None)
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
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn invoke_without_service_returns_none() {
    let root = Context::new();
    assert!(root.invoke::<Foo>(None).is_none());
}
