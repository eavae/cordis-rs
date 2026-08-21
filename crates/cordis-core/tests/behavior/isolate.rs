//! Ported cases from `packages/core/tests/isolate.spec.ts`.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{
    Context, Effect, EventFilter, EventOptions, Plugin, Service, event_listener, sync_disposer,
};

#[derive(Debug)]
struct Foo {
    bar: i32,
}

impl Service for Foo {
    const NAME: &'static str = "foo";
}

fn plugin_with_inject(callback: Arc<AtomicU32>, dispose_count: Arc<AtomicU32>) -> Plugin {
    Plugin {
        is_group: false,
        name: None,
        inject: vec![("foo".to_string(), None)],
        apply: Arc::new(
            move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                callback.store(callback.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                let dispose_count = dispose_count.clone();
                Effect::Disposer(sync_disposer(move || {
                    dispose_count.store(dispose_count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                }))
            },
        ),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn isolation_isolated_context() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback = Arc::new(AtomicU32::new(0));
            let dispose_count = Arc::new(AtomicU32::new(0));
            let plugin = plugin_with_inject(callback.clone(), dispose_count.clone());

            let root_fiber = root.plugin(&plugin, None);
            let ctx1 = root.isolate("foo", Arc::from("symbol-1"));
            let ctx1_fiber = ctx1.plugin(&plugin, None);
            let ctx2 = root.isolate("foo", Arc::from("symbol-2"));
            let ctx2_fiber = ctx2.plugin(&plugin, None);

            let dispose0 = root.provide::<Foo>(Arc::new(Foo { bar: 100 })).unwrap();
            root_fiber.wait().await.unwrap();
            assert_eq!(root.get::<Foo>().unwrap().bar, 100);
            assert!(ctx1.get::<Foo>().is_none());
            assert!(ctx2.get::<Foo>().is_none());
            assert_eq!(
                callback.load(Ordering::SeqCst),
                1,
                "only the root fiber applies"
            );
            assert_eq!(dispose_count.load(Ordering::SeqCst), 0);

            let dispose1 = ctx1.provide::<Foo>(Arc::new(Foo { bar: 200 })).unwrap();
            ctx1_fiber.wait().await.unwrap();
            assert_eq!(root.get::<Foo>().unwrap().bar, 100);
            assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
            assert!(ctx2.get::<Foo>().is_none());
            assert_eq!(callback.load(Ordering::SeqCst), 2);
            assert_eq!(dispose_count.load(Ordering::SeqCst), 0);

            dispose0.dispose().await.unwrap();
            root_fiber.wait().await.unwrap();
            assert!(root.get::<Foo>().is_none());
            assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
            assert!(ctx2.get::<Foo>().is_none());
            assert_eq!(callback.load(Ordering::SeqCst), 2);
            assert_eq!(dispose_count.load(Ordering::SeqCst), 1);

            let dispose2 = ctx2.provide::<Foo>(Arc::new(Foo { bar: 300 })).unwrap();
            ctx2_fiber.wait().await.unwrap();
            assert!(root.get::<Foo>().is_none());
            assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
            assert_eq!(ctx2.get::<Foo>().unwrap().bar, 300);
            assert_eq!(callback.load(Ordering::SeqCst), 3);
            assert_eq!(dispose_count.load(Ordering::SeqCst), 1);

            dispose1.dispose().await.unwrap();
            dispose2.dispose().await.unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn isolation_shared_label() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback = Arc::new(AtomicU32::new(0));
            let dispose_count = Arc::new(AtomicU32::new(0));
            let plugin = plugin_with_inject(callback.clone(), dispose_count.clone());

            let label = Arc::<str>::from("test");
            let root_fiber = root.plugin(&plugin, None);
            let ctx1 = root.isolate("foo", label.clone());
            let ctx1_fiber = ctx1.plugin(&plugin, None);
            let ctx2 = root.isolate("foo", label.clone());
            let ctx2_fiber = ctx2.plugin(&plugin, None);
            tokio::task::yield_now().await;
            assert_eq!(callback.load(Ordering::SeqCst), 0);

            let dispose0 = root.provide::<Foo>(Arc::new(Foo { bar: 100 })).unwrap();
            root_fiber.wait().await.unwrap();
            assert_eq!(root.get::<Foo>().unwrap().bar, 100);
            assert!(ctx1.get::<Foo>().is_none());
            assert!(ctx2.get::<Foo>().is_none());
            assert_eq!(callback.load(Ordering::SeqCst), 1);
            assert_eq!(dispose_count.load(Ordering::SeqCst), 0);

            let dispose12 = ctx1.provide::<Foo>(Arc::new(Foo { bar: 200 })).unwrap();
            ctx1_fiber.wait().await.unwrap();
            ctx2_fiber.wait().await.unwrap();
            assert_eq!(root.get::<Foo>().unwrap().bar, 100);
            assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
            assert_eq!(ctx2.get::<Foo>().unwrap().bar, 200);
            assert_eq!(
                callback.load(Ordering::SeqCst),
                3,
                "both isolated fibers share the label"
            );
            assert_eq!(dispose_count.load(Ordering::SeqCst), 0);

            dispose12.dispose().await.unwrap();
            ctx1_fiber.wait().await.unwrap();
            ctx2_fiber.wait().await.unwrap();
            assert_eq!(root.get::<Foo>().unwrap().bar, 100);
            assert!(ctx1.get::<Foo>().is_none());
            assert!(ctx2.get::<Foo>().is_none());
            assert_eq!(callback.load(Ordering::SeqCst), 3);
            assert_eq!(dispose_count.load(Ordering::SeqCst), 2);

            dispose0.dispose().await.unwrap();
        })
        .await;
}

/// The service `filter` semantics: an event emitted from a service only
/// reaches listeners whose context shares the service's isolate label.
struct ServiceEventFilter {
    provider_ctx: Context,
}

impl EventFilter for ServiceEventFilter {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn filter(&self, hook_ctx: &Context) -> bool {
        hook_ctx.isolate_label("foo") == self.provider_ctx.isolate_label("foo")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn isolation_isolated_event() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let ctx = root.isolate("foo", Arc::from("symbol-1"));
            let outer = Arc::new(AtomicU32::new(0));
            let inner = Arc::new(AtomicU32::new(0));
            drop(
                root.on(
                    "custom-event",
                    event_listener({
                        let outer = outer.clone();
                        move |_| outer.store(outer.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
            );
            drop(
                ctx.on(
                    "custom-event",
                    event_listener({
                        let inner = inner.clone();
                        move |_| inner.store(inner.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
            );

            let fiber = ctx.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                        drop(ctx.provide::<Foo>(Arc::new(Foo { bar: 1 })).unwrap());
                        // `ctx.emit(this, event)` with the service as thisArg.
                        let filter = ServiceEventFilter {
                            provider_ctx: ctx.clone(),
                        };
                        ctx.emit_with("custom-event", &[], &filter);
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();

            assert_eq!(outer.load(Ordering::SeqCst), 0);
            assert_eq!(inner.load(Ordering::SeqCst), 1);
        })
        .await;
}
