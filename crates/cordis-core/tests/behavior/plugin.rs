//! Ported cases from `packages/core/tests/plugin.spec.ts` (story card B4).

use std::cell::Cell;
use std::rc::Rc;

use cordis_core::{
    Context, Effect, EventOptions, Plugin, RegistryService, event_listener, sync_disposer,
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
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
            let apply = {
                let calls = calls.clone();
                Rc::new(move |_ctx: &Context, config: &Rc<dyn std::any::Any>| {
                    let options = config.downcast_ref::<Options>().expect("config").foo;
                    calls.borrow_mut().push(options);
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
                Some(Rc::new(Options { foo: "bar" })),
            );
            fiber.wait().await.unwrap();
            assert_eq!(calls.borrow().as_slice(), &["bar"]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn apply_object_plugin() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
            // The `Plugin` struct is the Rust equivalent of the TS object
            // plugin form `{ apply, name, inject }`.
            let plugin = Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: {
                    let calls = calls.clone();
                    Rc::new(move |_ctx: &Context, config: &Rc<dyn std::any::Any>| {
                        let bar = config.downcast_ref::<BarOptions>().expect("config").bar;
                        calls.borrow_mut().push(bar);
                        Effect::None
                    })
                },
            };
            let fiber = root.plugin(&plugin, Some(Rc::new(BarOptions { bar: "foo" })));
            fiber.wait().await.unwrap();
            assert_eq!(calls.borrow().as_slice(), &["foo"]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn inactive_context() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let other_calls = Rc::new(Cell::new(0u32));
            let other = Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: {
                    let other_calls = other_calls.clone();
                    Rc::new(move |_ctx: &Context, _config: &Rc<dyn std::any::Any>| {
                        other_calls.set(other_calls.get() + 1);
                        Effect::None
                    })
                },
            };
            let other_for_disposer = other.apply.clone();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(move |ctx: &Context, _config: &Rc<dyn std::any::Any>| {
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
            assert_eq!(other_calls.get(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn context_inspect() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            assert_eq!(format!("{root:?}"), "Context <root>");

            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|ctx: &Context, _config| {
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
                    apply: Rc::new(|ctx: &Context, _config| {
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
                    apply: Rc::new(|ctx: &Context, _config| {
                        assert_eq!(format!("{ctx:?}"), "Context <bar>");
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
async fn ctx_registry_queries() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let registry = root.get::<RegistryService>().unwrap();
            assert_eq!(registry.size(), 0);
            let _ = registry.keys();
            let _ = registry.values();

            let plugin = Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: Rc::new(|_ctx, _config| Effect::None),
            };
            let fiber = root.plugin(&plugin, None);
            fiber.wait().await.unwrap();
            assert!(registry.has(&plugin));
            assert_eq!(registry.size(), 1);
            assert_eq!(registry.keys().len(), 1);
            assert_eq!(registry.values().len(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn nested_plugins() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback_hit = Rc::new(Cell::new(0u32));
            let listener = {
                let callback_hit = callback_hit.clone();
                move |_args: &[Rc<dyn std::any::Any>]| {
                    callback_hit.set(callback_hit.get() + 1);
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
                apply: Rc::new(move |ctx: &Context, _config| {
                    let callback_hit = callback_hit4.clone();
                    drop(
                        ctx.on(
                            "custom-event",
                            event_listener(move |_| {
                                callback_hit.set(callback_hit.get() + 1);
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
                apply: Rc::new(move |ctx: &Context, _config| {
                    let callback_hit = callback_hit3.clone();
                    drop(
                        ctx.on(
                            "custom-event",
                            event_listener(move |_| {
                                callback_hit.set(callback_hit.get() + 1);
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
                apply: Rc::new(move |ctx: &Context, _config| {
                    let callback_hit = callback_hit2.clone();
                    drop(
                        ctx.on(
                            "custom-event",
                            event_listener(move |_| {
                                callback_hit.set(callback_hit.get() + 1);
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
            assert_eq!(callback_hit.get(), 0);
            root.emit("custom-event", &[]);
            assert_eq!(callback_hit.get(), 4);

            callback_hit.set(0);
            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(registry.size(), 0);
            root.emit("custom-event", &[]);
            assert_eq!(callback_hit.get(), 1);

            // Subsequent disposal is a no-op.
            callback_hit.set(0);
            fiber.dispose().await;
            assert_eq!(registry.size(), 0);
            root.emit("custom-event", &[]);
            assert_eq!(callback_hit.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn compare_snapshot_after_registry_delete() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback_hit = Rc::new(Cell::new(0u32));
            drop(
                root.on(
                    "custom-event",
                    event_listener({
                        let callback_hit = callback_hit.clone();
                        move |_| callback_hit.set(callback_hit.get() + 1)
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
                    Rc::new(move |ctx: &Context, _config| {
                        let callback_hit = callback_hit.clone();
                        drop(
                            ctx.on(
                                "custom-event",
                                event_listener(move |_| {
                                    callback_hit.set(callback_hit.get() + 1);
                                }),
                                EventOptions::default(),
                            )
                            .unwrap(),
                        );
                        Effect::None
                    })
                },
            };

            let before = callback_hit.get();
            let fiber = root.plugin(&plugin, None);
            fiber.wait().await.unwrap();
            root.emit("custom-event", &[]);
            assert_eq!(callback_hit.get(), before + 2, "root + plugin listener");

            let registry = root.get::<RegistryService>().unwrap();
            registry.delete(&plugin);
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            callback_hit.set(0);
            root.emit("custom-event", &[]);
            assert_eq!(callback_hit.get(), 1, "only the root listener remains");

            let fiber = root.plugin(&plugin, None);
            fiber.wait().await.unwrap();
            callback_hit.set(0);
            root.emit("custom-event", &[]);
            assert_eq!(callback_hit.get(), 2);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn root_dispose() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let dispose_called = Rc::new(Cell::new(0u32));
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: {
                        let dispose_called = dispose_called.clone();
                        Rc::new(move |_ctx: &Context, _config| {
                            let dispose_called = dispose_called.clone();
                            Effect::Disposer(sync_disposer(move || {
                                dispose_called.set(dispose_called.get() + 1);
                            }))
                        })
                    },
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(root.fiber().uid.get(), Some(0));
            assert_eq!(fiber.uid.get(), Some(1));
            assert_eq!(dispose_called.get(), 0);
            assert_eq!(root.fiber().effect_count(), 1);

            root.fiber().dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(root.fiber().uid.get(), Some(0));
            assert_eq!(fiber.uid.get(), None);
            assert_eq!(dispose_called.get(), 1);
            assert_eq!(root.fiber().effect_count(), 0);

            root.fiber().dispose().await;
            assert_eq!(root.fiber().uid.get(), Some(0));
            assert_eq!(fiber.uid.get(), None);
            assert_eq!(dispose_called.get(), 1);
            assert_eq!(root.fiber().effect_count(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_init_equivalent() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // TS `Service.init` (constructor plugins returning a stop
            // disposer) maps to the apply callback returning a disposer.
            let root = Context::new();
            let start = Rc::new(Cell::new(0u32));
            let stop = Rc::new(Cell::new(0u32));
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: {
                        let start = start.clone();
                        let stop = stop.clone();
                        Rc::new(move |_ctx: &Context, _config| {
                            start.set(start.get() + 1);
                            let stop = stop.clone();
                            Effect::Disposer(sync_disposer(move || {
                                stop.set(stop.get() + 1);
                            }))
                        })
                    },
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(start.get(), 1);
            assert_eq!(stop.get(), 0);

            fiber.dispose().await;
            assert_eq!(start.get(), 1);
            assert_eq!(stop.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn shared_runtime_multiple_fibers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let apply = Rc::new(|_ctx: &Context, _config: &Rc<dyn std::any::Any>| Effect::None);
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
        })
        .await;
}
