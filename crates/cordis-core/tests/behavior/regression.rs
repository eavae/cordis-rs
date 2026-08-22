//! Core integration regression: fiber × events × registry × logger
//! interacting in one scenario.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{
    Context, Effect, EventOptions, LoggerLevel, Plugin, Service, SimpleExporter, event_listener,
    sync_disposer,
};

#[derive(Debug)]
struct Counter {
    value: AtomicU32,
}

impl Service for Counter {
    const NAME: &'static str = "counter";
}

#[tokio::test(flavor = "current_thread")]
async fn core_integration_scenario() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let logs = Arc::new(Mutex::new(Vec::new()));
            let logger = root.logger();
            drop(
                logger
                    .exporter(Arc::new(SimpleExporter {
                        colors: 0,
                        max_length: 10240,
                        levels: Some(Arc::new(std::collections::HashMap::from([(
                            "default".to_string(),
                            LoggerLevel::Debug,
                        )]))),
                        formatters: None,
                        handler: {
                            let logs = logs.clone();
                            Arc::new(move |message| logs.lock().push(message.args[0].inspect()))
                        },
                    }))
                    .unwrap(),
            );

            // A provider service, an event listener and a consumer plugin.
            let counter_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(
                            ctx.provide::<Counter>(Arc::new(Counter {
                                value: AtomicU32::new(0),
                            }))
                            .unwrap(),
                        );
                        Effect::None
                    }),
                },
                None,
            );
            counter_fiber.wait().await.unwrap();

            let event_hits = Arc::new(AtomicU32::new(0));
            drop(
                root.on(
                    "tick",
                    event_listener({
                        let event_hits = event_hits.clone();
                        move |_| {
                            event_hits
                                .store(event_hits.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
                        }
                    }),
                    EventOptions::default(),
                )
                .unwrap(),
            );

            let applied = Arc::new(AtomicU32::new(0));
            let event_hits_apply = event_hits.clone();
            let consumer_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: vec![("counter".to_string(), None)],
                    apply: {
                        let applied = applied.clone();
                        Arc::new(move |ctx: &Context, _config| {
                            applied.store(applied.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            let counter = ctx.get::<Counter>().expect("counter");
                            counter
                                .value
                                .store(counter.value.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            let logger = ctx.logger().named("consumer");
                            logger.debug("counter incremented");
                            let event_hits = event_hits_apply.clone();
                            Effect::Disposer(sync_disposer(move || {
                                event_hits.store(
                                    event_hits.load(Ordering::SeqCst) + 10,
                                    Ordering::SeqCst,
                                );
                            }))
                        })
                    },
                },
                None,
            );
            consumer_fiber.wait().await.unwrap();

            root.emit("tick", &[]);
            assert_eq!(applied.load(Ordering::SeqCst), 1);
            assert_eq!(event_hits.load(Ordering::SeqCst), 1);
            assert_eq!(
                root.get::<Counter>().unwrap().value.load(Ordering::SeqCst),
                1
            );
            assert!(
                logs.lock()
                    .iter()
                    .any(|line| line.contains("counter incremented")),
                "{:?}",
                logs.lock()
            );

            // Disposing the provider unloads the consumer and its disposer.
            counter_fiber.dispose().await;
            consumer_fiber.wait().await.unwrap();
            assert_eq!(event_hits.load(Ordering::SeqCst), 11);
            assert!(root.get::<Counter>().is_none());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn nested_lifecycle_leaves_no_trace() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let registry = root.get::<cordis_core::RegistryService>().unwrap();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(
                            ctx.on("evt", event_listener(|_| {}), EventOptions::default())
                                .unwrap(),
                        );
                        let _ = ctx.plugin(
                            &Plugin {
                                is_group: false,
                                name: None,
                                inject: Vec::new(),
                                apply: Arc::new(|_ctx, _config| Effect::None),
                            },
                            None,
                        );
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(registry.size(), 2);

            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(registry.size(), 0);
            // The inert registration wrapper stays on the root fiber
            // (mirrors TS, where the disposed effect remains in
            // `_disposables` with its epoch gate consumed).
            assert_eq!(root.fiber().effect_count(), 1);
        })
        .await;
}
