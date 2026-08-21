//! Ported cases from `packages/core/tests/fiber.spec.ts`.
//!
//! The TS runtime drives pending promises automatically; the Rust runtime
//! schedules fiber state-machine tasks on a `LocalSet` (`spawn_local`), so
//! these tests run inside one and observe the same state transitions.

use std::any::Any;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{
    Context, Effect, EventOptions, FiberState, LoggerService, Plugin, Service, async_disposer,
    event_listener,
};

#[derive(Debug)]
struct Foo;

impl Service for Foo {
    const NAME: &'static str = "foo";
}

#[derive(Debug)]
struct Msg {
    msg: &'static str,
}

#[derive(Debug)]
struct PluginConfig {
    foo: bool,
}

#[derive(Debug)]
struct ProviderConfig {
    value: i32,
}

#[derive(Debug)]
struct ConsumerConfig {
    mode: &'static str,
}

#[derive(Debug)]
struct Provider {
    value: i32,
}

impl Service for Provider {
    const NAME: &'static str = "provider";
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_inertia_lock_1() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let dispose = root.provide_str("foo", Arc::new(1i32)).unwrap();
            let timers_for_cb = timers.clone();
            let fiber = root.inject(
                &["foo"],
                Arc::new(move |_ctx, _config| {
                    let timers = timers_for_cb.clone();
                    Effect::Async(Box::pin(async move {
                        timers.sleep(1000).await;
                        let timers = timers.clone();
                        Ok(async_disposer(move || async move {
                            timers.sleep(1000).await;
                            Ok(())
                        }))
                    }))
                }),
            );
            // Drive the reload task to register its apply sleep at t=0.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Loading);

            dispose.dispose().await.unwrap();
            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Loading);

            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Unloading);

            drop(root.provide_str("foo", Arc::new(1i32)).unwrap());
            tokio::task::yield_now().await;
            timers.advance(1000).await;
            assert_eq!(fiber.state(), FiberState::Loading);

            timers.advance(1000).await;
            assert_eq!(fiber.state(), FiberState::Active);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_inertia_lock_2() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let dispose = root.provide_str("foo", Arc::new(1i32)).unwrap();
            let timers_for_cb = timers.clone();
            let fiber = root.inject(
                &["foo"],
                Arc::new(move |_ctx, _config| {
                    let timers = timers_for_cb.clone();
                    Effect::Async(Box::pin(async move {
                        timers.sleep(1000).await;
                        let timers = timers.clone();
                        Ok(async_disposer(move || async move {
                            timers.sleep(1000).await;
                            Ok(())
                        }))
                    }))
                }),
            );
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Loading);

            dispose.dispose().await.unwrap();
            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Loading);

            drop(root.provide_str("foo", Arc::new(2i32)).unwrap());
            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Active);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_inertia_lock_3() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let provider = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx, _config| {
                        drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            provider.wait().await.unwrap();

            let timers_for_cb = timers.clone();
            let fiber = root.inject(
                &["foo"],
                Arc::new(move |_ctx, _config| {
                    let timers = timers_for_cb.clone();
                    Effect::Async(Box::pin(async move {
                        timers.sleep(1000).await;
                        let timers = timers.clone();
                        Ok(async_disposer(move || async move {
                            timers.sleep(1000).await;
                            Ok(())
                        }))
                    }))
                }),
            );
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            timers.advance(400).await;
            assert_eq!(fiber.state(), FiberState::Loading);

            timers.advance(1000).await;
            assert_eq!(fiber.state(), FiberState::Active);

            provider.dispose().await;
            tokio::task::yield_now().await;
            timers.advance(2000).await;
            assert_eq!(fiber.state(), FiberState::Pending);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_plugin_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback_hit = Arc::new(AtomicU32::new(0));
            let apply = {
                let callback_hit = callback_hit.clone();
                Arc::new(move |ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                    let callback_hit = callback_hit.clone();
                    drop(
                        ctx.on(
                            "custom",
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
                    let config = config.downcast_ref::<PluginConfig>().expect("config");
                    if !config.foo {
                        Effect::Error(Box::new(std::io::Error::other("plugin error")))
                    } else {
                        Effect::None
                    }
                })
            };

            let fiber1 = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                Some(Arc::new(PluginConfig { foo: false })),
            );
            let fiber2 = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                Some(Arc::new(PluginConfig { foo: true })),
            );

            tokio::task::yield_now().await;
            assert!(fiber1.wait().await.is_err());
            assert_eq!(fiber1.state(), FiberState::Failed);
            fiber2.wait().await.unwrap();
            assert_eq!(fiber2.state(), FiberState::Active);
            let logger = root.get::<LoggerService>().unwrap();
            assert_eq!(
                logger.error_count(),
                1,
                "apply error must be logged exactly once"
            );

            root.emit("custom", &[]);
            assert_eq!(callback_hit.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_dispose_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let dispose_called = Arc::new(AtomicU32::new(0));
            let apply = {
                let dispose_called = dispose_called.clone();
                Arc::new(
                    move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                        let dispose_called = dispose_called.clone();
                        Effect::Disposer(async_disposer(move || async move {
                            dispose_called
                                .store(dispose_called.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            Err::<(), Box<dyn std::error::Error + Send + Sync>>(Box::new(
                                std::io::Error::other("test"),
                            ))
                        }))
                    },
                )
            };
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(dispose_called.load(Ordering::SeqCst), 0);

            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
            assert_eq!(root.get::<LoggerService>().unwrap().error_count(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_update_config_on_wrapped_fiber() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let apply = {
                let calls = calls.clone();
                Arc::new(move |_ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                    let msg = config.downcast_ref::<Msg>().expect("config").msg;
                    calls.lock().unwrap().push(msg);
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
                Some(Arc::new(Msg { msg: "hello" })),
            );
            fiber.wait().await.unwrap();
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert_eq!(calls.lock().unwrap()[0], "hello");

            fiber
                .update(Some(Arc::new(Msg { msg: "world" })))
                .await
                .unwrap();
            assert_eq!(calls.lock().unwrap().len(), 2);
            assert_eq!(calls.lock().unwrap()[1], "world");

            fiber
                .update(Some(Arc::new(Msg { msg: "!!!" })))
                .await
                .unwrap();
            assert_eq!(calls.lock().unwrap().len(), 3);
            assert_eq!(calls.lock().unwrap()[2], "!!!");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_restart_wrapped_fiber() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let calls = Arc::new(AtomicU32::new(0));
            let apply = {
                let calls = calls.clone();
                Arc::new(
                    move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                        calls.store(calls.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Effect::None
                    },
                )
            };
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);

            fiber.restart().await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(fiber.state(), FiberState::Active);
        })
        .await;
}

/// A panicking background task must not hang `wait_task`: the completion
/// flag has to be set on the error path too.
#[tokio::test(flavor = "current_thread")]
async fn wait_task_survives_panicking_background_task() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let handle = root
                .fiber()
                .effect(
                    || {
                        Effect::Async(Box::pin(async {
                            panic!("boom");
                        }))
                    },
                    "panicking effect",
                )
                .expect("effect");
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(500), handle.wait_task())
                    .await;
            assert!(
                result.is_ok(),
                "wait_task must resolve when the background task panics"
            );
            assert!(
                result.unwrap().is_err(),
                "a panicked task must surface as an error"
            );
        })
        .await;
}

/// A panicking apply callback must not hang `Fiber::wait`: the inertia lock
/// has to be released on the error path too.
#[tokio::test(flavor = "current_thread")]
async fn wait_survives_panicking_apply_callback() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|_ctx: &Context, _config| panic!("boom")),
                },
                None,
            );
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(500), fiber.wait()).await;
            assert!(
                result.is_ok(),
                "wait must resolve when the apply callback panics"
            );
            assert!(
                result.unwrap().is_err(),
                "a panicked apply must surface as an error"
            );
        })
        .await;
}

/// A panicking async apply effect must not hang `Fiber::wait` either.
#[tokio::test(flavor = "current_thread")]
async fn wait_survives_panicking_async_apply() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|_ctx: &Context, _config| {
                        Effect::Async(Box::pin(async {
                            panic!("boom");
                        }))
                    }),
                },
                None,
            );
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(500), fiber.wait()).await;
            assert!(
                result.is_ok(),
                "wait must resolve when the async apply panics"
            );
            assert!(
                result.unwrap().is_err(),
                "a panicked async apply must surface as an error"
            );
        })
        .await;
}

/// A panicking effect callback must be contained at the effect boundary:
/// `effect` returns an error instead of unwinding into the caller (the TS
/// reference rethrows inside `fiber.effect`, so callers observe the failure
/// through the returned result).
#[tokio::test(flavor = "current_thread")]
async fn effect_callback_panic_is_contained() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                root.fiber().effect(|| panic!("boom"), "panicking")
            }));
            let result = result.expect("effect callback panic must not unwind");
            assert!(
                result.is_err(),
                "a panicked effect callback must surface as an error"
            );
        })
        .await;
}

/// A panicking effect callback registered from an event listener must not
/// unwind into the dispatch machinery: the listener observes the error and
/// later listeners still run.
#[tokio::test(flavor = "current_thread")]
async fn effect_callback_panic_contained_in_listener() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let seen = Arc::new(AtomicU32::new(0));
            {
                let root_for_listener = root.clone();
                root.on(
                    "ping",
                    event_listener(move |_| {
                        let _ = root_for_listener.effect(|| panic!("boom"), "panicking");
                    }),
                    EventOptions::default(),
                )
                .unwrap();
            }
            {
                let seen = seen.clone();
                root.on(
                    "ping",
                    event_listener(move |_| {
                        seen.store(1, Ordering::SeqCst);
                    }),
                    EventOptions::default(),
                )
                .unwrap();
            }
            let emitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                root.emit("ping", &[]);
            }));
            assert!(
                emitted.is_ok(),
                "effect callback panic must not escape into emit"
            );
            assert_eq!(
                seen.load(Ordering::SeqCst),
                1,
                "later listeners must still run"
            );
        })
        .await;
}

/// A panicking effect callback inside an apply must fail only the owning
/// fiber: its registrations are unloaded and unrelated fibers stay active.
#[tokio::test(flavor = "current_thread")]
async fn effect_callback_panic_fails_owning_fiber_only() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let other_applied = Arc::new(AtomicU32::new(0));
            let fiber_a = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        // Register a service first: it must be unloaded when
                        // the fiber fails below.
                        drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                        // An unhandled effect failure (mirrors the TS
                        // rethrow) fails the whole apply.
                        ctx.effect(|| panic!("boom"), "panicking").unwrap();
                        Effect::None
                    }),
                },
                None,
            );
            let fiber_b = {
                let other_applied = other_applied.clone();
                root.plugin(
                    &Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: Arc::new(move |_ctx: &Context, _config| {
                            other_applied.store(1, Ordering::SeqCst);
                            Effect::None
                        }),
                    },
                    None,
                )
            };
            let result =
                tokio::time::timeout(std::time::Duration::from_millis(500), fiber_a.wait()).await;
            assert!(
                result.is_ok(),
                "wait must resolve after an effect callback panic"
            );
            assert!(
                result.unwrap().is_err(),
                "a panicked effect callback must surface as an error"
            );
            assert_eq!(fiber_a.state(), FiberState::Failed);
            assert!(
                root.get_str_non_strict("foo").is_none(),
                "a failed fiber must unload its services"
            );
            fiber_b.wait().await.unwrap();
            assert_eq!(other_applied.load(Ordering::SeqCst), 1);
            assert_eq!(fiber_b.state(), FiberState::Active);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_update_config_while_injected_service_reloads() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let applied = Arc::new(Mutex::new(Vec::new()));

            let provider_apply = Arc::new(|ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                let value = config
                    .downcast_ref::<ProviderConfig>()
                    .expect("config")
                    .value;
                drop(
                    ctx.provide::<Provider>(Arc::new(Provider { value }))
                        .unwrap(),
                );
                Effect::None
            });
            let provider = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: provider_apply,
                },
                Some(Arc::new(ProviderConfig { value: 1 })),
            );
            provider.wait().await.unwrap();

            let consumer_apply = {
                let applied = applied.clone();
                Arc::new(move |ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                    let mode = config
                        .downcast_ref::<ConsumerConfig>()
                        .expect("config")
                        .mode;
                    let value = ctx.get::<Provider>().expect("provider").value;
                    applied.lock().unwrap().push((value, mode));
                    Effect::None
                })
            };
            let consumer = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: vec![("provider".to_string(), None)],
                    apply: consumer_apply,
                },
                Some(Arc::new(ConsumerConfig { mode: "old" })),
            );
            consumer.wait().await.unwrap();
            assert_eq!(applied.lock().unwrap().as_slice(), &[(1, "old")]);

            let provider_update = provider.update(Some(Arc::new(ProviderConfig { value: 2 })));
            let consumer_update = consumer.update(Some(Arc::new(ConsumerConfig { mode: "new" })));
            let (provider_result, consumer_result) = tokio::join!(provider_update, consumer_update);
            provider_result.unwrap();
            consumer_result.unwrap();

            assert_eq!(
                applied.lock().unwrap().as_slice(),
                &[(1, "old"), (2, "new")]
            );
            assert_eq!(consumer.state(), FiberState::Active);
        })
        .await;
}
