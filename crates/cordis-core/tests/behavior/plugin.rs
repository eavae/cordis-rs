//! Ported cases from `packages/core/tests/plugin.spec.ts`.

use parking_lot::Mutex;
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{
    Context, Effect, EventOptions, FiberState, Plugin, PluginOutput, RegistryService,
    event_listener, plugin_async, plugin_sync, sync_disposer,
};

#[derive(Debug)]
struct Options {
    foo: &'static str,
}

#[derive(Debug)]
struct BarOptions {
    bar: &'static str,
}

#[tokio::test(flavor = "current_thread")]
async fn apply_functional_plugin() {
    async {
        let root = Context::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let apply = {
            let calls = calls.clone();
            Arc::new(move |_ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                let options = config.downcast_ref::<Options>().expect("config").foo;
                calls.lock().push(options);
                Effect::None
            })
        };
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: apply.clone(),
            },
            Some(Arc::new(Options { foo: "bar" })),
        );
        fiber.wait().await.unwrap();
        assert_eq!(calls.lock().as_slice(), &["bar"]);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn apply_object_plugin() {
    async {
        let root = Context::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        // The `Plugin` struct is the Rust equivalent of the TS object
        // plugin form `{ apply, name, inject }`.
        let plugin = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: {
                let calls = calls.clone();
                Arc::new(move |_ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                    let bar = config.downcast_ref::<BarOptions>().expect("config").bar;
                    calls.lock().push(bar);
                    Effect::None
                })
            },
        };
        let fiber = root.plugin(&plugin, Some(Arc::new(BarOptions { bar: "foo" })));
        fiber.wait().await.unwrap();
        assert_eq!(calls.lock().as_slice(), &["foo"]);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn inactive_context() {
    async {
        let root = Context::new();
        let other_calls = Arc::new(AtomicU32::new(0));
        let other = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: {
                let other_calls = other_calls.clone();
                Arc::new(
                    move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                        other_calls.store(other_calls.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Effect::None
                    },
                )
            },
        };
        let other_for_disposer = other.apply.clone();
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: Arc::new(move |ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                    let ctx = ctx.clone();
                    let other = Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: other_for_disposer.clone(),
                    };
                    Effect::Disposer(sync_disposer(move || {
                        let panicked =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                let _ = ctx.plugin(&other, None);
                            }));
                        assert!(panicked.is_err(), "plugin on inactive context must fail");
                        assert!(
                            ctx.effect(|| Effect::None, "x").is_err(),
                            "effect on inactive context must fail"
                        );
                        assert!(
                            ctx.on(
                                "custom-event",
                                event_listener(|_| {}),
                                EventOptions::default(),
                            )
                            .is_err(),
                            "on on inactive context must fail"
                        );
                    }))
                }),
            },
            None,
        );
        fiber.wait().await.unwrap();
        fiber.dispose().await;
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn context_inspect() {
    async {
        let root = Context::new();
        assert_eq!(format!("{root:?}"), "Context <root>");

        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: Arc::new(|ctx: &Context, _config| {
                    assert_eq!(format!("{ctx:?}"), "Context <root>");
                    Effect::None
                }),
            },
            None,
        );
        fiber.wait().await.unwrap();

        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: Some("foo".to_string()),
                inject: Vec::new(),
                apply: Arc::new(|ctx: &Context, _config| {
                    assert_eq!(format!("{ctx:?}"), "Context <foo>");
                    Effect::None
                }),
            },
            None,
        );
        fiber.wait().await.unwrap();

        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: Some("bar".to_string()),
                inject: Vec::new(),
                apply: Arc::new(|ctx: &Context, _config| {
                    assert_eq!(format!("{ctx:?}"), "Context <bar>");
                    Effect::None
                }),
            },
            None,
        );
        fiber.wait().await.unwrap();
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn ctx_registry_queries() {
    async {
        let root = Context::new();
        let registry = root.get::<RegistryService>().unwrap();
        assert_eq!(registry.size(), 0);
        let _ = registry.keys();
        let _ = registry.values();

        let plugin = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: Arc::new(|_ctx, _config| Effect::None),
        };
        let fiber = root.plugin(&plugin, None);
        fiber.wait().await.unwrap();
        assert!(registry.has(&plugin));
        assert_eq!(registry.size(), 1);
        assert_eq!(registry.keys().len(), 1);
        assert_eq!(registry.values().len(), 1);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn nested_plugins() {
    async {
        let root = Context::new();
        let callback_hit = Arc::new(AtomicU32::new(0));
        let listener = {
            let callback_hit = callback_hit.clone();
            move |_args: &[Arc<dyn Any + Send + Sync>]| {
                callback_hit.store(callback_hit.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            }
        };
        drop(
            root.on(
                "custom-event",
                event_listener(listener),
                EventOptions::default(),
            )
            .unwrap(),
        );

        let callback_hit2 = callback_hit.clone();
        let callback_hit3 = callback_hit.clone();
        let callback_hit4 = callback_hit.clone();
        let nested2 = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: Arc::new(move |ctx: &Context, _config| {
                let callback_hit = callback_hit4.clone();
                drop(
                    ctx.on(
                        "custom-event",
                        event_listener(move |_| {
                            callback_hit
                                .store(callback_hit.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }),
                        EventOptions::default(),
                    )
                    .unwrap(),
                );
                Effect::None
            }),
        };
        let nested1 = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: Arc::new(move |ctx: &Context, _config| {
                let callback_hit = callback_hit3.clone();
                drop(
                    ctx.on(
                        "custom-event",
                        event_listener(move |_| {
                            callback_hit
                                .store(callback_hit.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }),
                        EventOptions::default(),
                    )
                    .unwrap(),
                );
                let _ = ctx.plugin(&nested2, None);
                Effect::None
            }),
        };
        let outer = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: Arc::new(move |ctx: &Context, _config| {
                let callback_hit = callback_hit2.clone();
                drop(
                    ctx.on(
                        "custom-event",
                        event_listener(move |_| {
                            callback_hit
                                .store(callback_hit.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }),
                        EventOptions::default(),
                    )
                    .unwrap(),
                );
                let _ = ctx.plugin(&nested1, None);
                Effect::None
            }),
        };

        let fiber = root.plugin(&outer, None);
        fiber.wait().await.unwrap();
        let registry = root.get::<RegistryService>().unwrap();
        assert_eq!(registry.size(), 3);
        assert_eq!(callback_hit.load(Ordering::SeqCst), 0);
        root.emit("custom-event", &[]);
        assert_eq!(callback_hit.load(Ordering::SeqCst), 4);

        callback_hit.store(0, Ordering::SeqCst);
        fiber.dispose().await;
        tokio::task::yield_now().await;
        assert_eq!(registry.size(), 0);
        root.emit("custom-event", &[]);
        assert_eq!(callback_hit.load(Ordering::SeqCst), 1);

        // Subsequent disposal is a no-op.
        callback_hit.store(0, Ordering::SeqCst);
        fiber.dispose().await;
        assert_eq!(registry.size(), 0);
        root.emit("custom-event", &[]);
        assert_eq!(callback_hit.load(Ordering::SeqCst), 1);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn compare_snapshot_after_registry_delete() {
    async {
        let root = Context::new();
        let callback_hit = Arc::new(AtomicU32::new(0));
        drop(
            root.on(
                "custom-event",
                event_listener({
                    let callback_hit = callback_hit.clone();
                    move |_| {
                        callback_hit
                            .store(callback_hit.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
                    }
                }),
                EventOptions::default(),
            )
            .unwrap(),
        );
        let plugin = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: {
                let callback_hit = callback_hit.clone();
                Arc::new(move |ctx: &Context, _config| {
                    let callback_hit = callback_hit.clone();
                    drop(
                        ctx.on(
                            "custom-event",
                            event_listener(move |_| {
                                callback_hit.store(
                                    callback_hit.load(Ordering::SeqCst) + 1,
                                    Ordering::SeqCst,
                                );
                            }),
                            EventOptions::default(),
                        )
                        .unwrap(),
                    );
                    Effect::None
                })
            },
        };

        let before = callback_hit.load(Ordering::SeqCst);
        let fiber = root.plugin(&plugin, None);
        fiber.wait().await.unwrap();
        root.emit("custom-event", &[]);
        assert_eq!(
            callback_hit.load(Ordering::SeqCst),
            before + 2,
            "root + plugin listener"
        );

        let registry = root.get::<RegistryService>().unwrap();
        registry.delete(&plugin);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        callback_hit.store(0, Ordering::SeqCst);
        root.emit("custom-event", &[]);
        assert_eq!(
            callback_hit.load(Ordering::SeqCst),
            1,
            "only the root listener remains"
        );

        let fiber = root.plugin(&plugin, None);
        fiber.wait().await.unwrap();
        callback_hit.store(0, Ordering::SeqCst);
        root.emit("custom-event", &[]);
        assert_eq!(callback_hit.load(Ordering::SeqCst), 2);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn root_dispose() {
    async {
        let root = Context::new();
        let dispose_called = Arc::new(AtomicU32::new(0));
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: {
                    let dispose_called = dispose_called.clone();
                    Arc::new(move |_ctx: &Context, _config| {
                        let dispose_called = dispose_called.clone();
                        Effect::Disposer(sync_disposer(move || {
                            dispose_called
                                .store(dispose_called.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }))
                    })
                },
            },
            None,
        );
        fiber.wait().await.unwrap();
        assert_eq!(root.fiber().uid(), Some(0));
        assert_eq!(fiber.uid(), Some(1));
        assert_eq!(dispose_called.load(Ordering::SeqCst), 0);
        assert_eq!(root.fiber().effect_count(), 1);

        root.fiber().dispose().await;
        tokio::task::yield_now().await;
        assert_eq!(root.fiber().uid(), Some(0));
        assert_eq!(fiber.uid(), None);
        assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
        assert_eq!(root.fiber().effect_count(), 0);

        root.fiber().dispose().await;
        assert_eq!(root.fiber().uid(), Some(0));
        assert_eq!(fiber.uid(), None);
        assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
        assert_eq!(root.fiber().effect_count(), 0);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_init_equivalent() {
    async {
        // TS `Service.init` (constructor plugins returning a stop
        // disposer) maps to the apply callback returning a disposer.
        let root = Context::new();
        let start = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicU32::new(0));
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: {
                    let start = start.clone();
                    let stop = stop.clone();
                    Arc::new(move |_ctx: &Context, _config| {
                        start.store(start.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        let stop = stop.clone();
                        Effect::Disposer(sync_disposer(move || {
                            stop.store(stop.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }))
                    })
                },
            },
            None,
        );
        fiber.wait().await.unwrap();
        assert_eq!(start.load(Ordering::SeqCst), 1);
        assert_eq!(stop.load(Ordering::SeqCst), 0);

        fiber.dispose().await;
        assert_eq!(start.load(Ordering::SeqCst), 1);
        assert_eq!(stop.load(Ordering::SeqCst), 1);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn shared_runtime_multiple_fibers() {
    async {
        let root = Context::new();
        let apply = Arc::new(|_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| Effect::None);
        let plugin = Plugin {
            is_group: false,
            name: Some("shared".to_string()),
            inject: Vec::new(),
            apply: apply.clone(),
        };
        let fiber1 = root.plugin(&plugin, None);
        let fiber2 = root.plugin(&plugin, None);
        fiber1.wait().await.unwrap();
        fiber2.wait().await.unwrap();

        let registry = root.get::<RegistryService>().unwrap();
        assert_eq!(registry.size(), 1, "same plugin shares one runtime");
        let runtimes = registry.values();
        assert_eq!(runtimes[0].fiber_count(), 2);

        fiber1.dispose().await;
        assert_eq!(registry.size(), 1);
        fiber2.dispose().await;
        assert_eq!(registry.size(), 0, "runtime removed after last fiber");
    }
    .await;
}

/// The `plugin_sync` adapter: typed config is delivered to the closure and
/// its `PluginOutput` disposer runs on unload.
#[tokio::test(flavor = "current_thread")]
async fn plugin_sync_typed_config_and_cleanup() {
    async {
        let root = Context::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let cleaned = Arc::new(AtomicU32::new(0));
        let spec = plugin_sync::<Options, _, _, _>("typed", Vec::<&str>::new(), {
            let seen = seen.clone();
            let cleaned = cleaned.clone();
            move |_ctx: &Context, config: &Arc<Options>| {
                seen.lock().push(config.foo);
                let cleaned = cleaned.clone();
                Ok(PluginOutput::infallible(move || {
                    cleaned.store(1, Ordering::SeqCst);
                }))
            }
        });

        let fiber = spec.register(&root, Some(Arc::new(Options { foo: "bar" })));
        fiber.wait().await.unwrap();
        assert_eq!(fiber.state(), FiberState::Active);
        assert_eq!(seen.lock().as_slice(), &["bar"]);

        fiber.dispose().await;
        tokio::task::yield_now().await;
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }
    .await;
}

/// A wrong-typed update on a `plugin_sync` plugin fails validation and leaves
/// the running instance untouched (the type check is attached to the spec).
#[tokio::test(flavor = "current_thread")]
async fn plugin_sync_wrong_config_type_keeps_running_instance() {
    async {
        let root = Context::new();
        let applies = Arc::new(AtomicU32::new(0));
        let spec = plugin_sync::<Options, _, _, _>("typed", Vec::<&str>::new(), {
            let applies = applies.clone();
            move |_ctx: &Context, _config: &Arc<Options>| {
                applies.store(applies.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                Ok(PluginOutput::none())
            }
        });

        let fiber = spec.register(&root, Some(Arc::new(Options { foo: "ok" })));
        fiber.wait().await.unwrap();
        assert_eq!(applies.load(Ordering::SeqCst), 1);

        let error = fiber.update(Some(Arc::new(7_u32))).await.unwrap_err();
        assert!(error.to_string().contains("invalid config"), "{error}");
        assert_eq!(fiber.state(), FiberState::Active);
        assert_eq!(
            applies.load(Ordering::SeqCst),
            1,
            "the running instance is untouched"
        );
    }
    .await;
}

/// The `plugin_async` adapter: the fiber stays Loading until the apply future
/// resolves, and the returned disposer runs on unload.
#[tokio::test(flavor = "current_thread")]
async fn plugin_async_waits_for_apply_and_cleans_up() {
    async {
        let root = Context::new();
        let ready = Arc::new(tokio::sync::Notify::new());
        let cleaned = Arc::new(AtomicU32::new(0));
        let spec = plugin_async::<Options, _, _, _, _>("async", Vec::<&str>::new(), {
            let ready = ready.clone();
            let cleaned = cleaned.clone();
            move |_ctx: &Context, config: &Arc<Options>| {
                assert_eq!(config.foo, "bar");
                let ready = ready.clone();
                let cleaned = cleaned.clone();
                async move {
                    ready.notified().await;
                    Ok(PluginOutput::infallible(move || {
                        cleaned.store(1, Ordering::SeqCst);
                    }))
                }
            }
        });

        let fiber = spec.register(&root, Some(Arc::new(Options { foo: "bar" })));
        tokio::task::yield_now().await;
        assert_eq!(fiber.state(), FiberState::Loading);

        ready.notify_one();
        fiber.wait().await.unwrap();
        assert_eq!(fiber.state(), FiberState::Active);

        fiber.dispose().await;
        tokio::task::yield_now().await;
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    }
    .await;
}
