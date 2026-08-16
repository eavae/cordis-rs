//! Ported cases from `packages/core/tests/fiber.spec.ts` (story card B2).
//!
//! The TS runtime drives pending promises automatically; the Rust runtime
//! schedules fiber state-machine tasks on a `LocalSet` (`spawn_local`), so
//! these tests run inside one and observe the same state transitions.

use std::cell::Cell;
use std::rc::Rc;

use cordis_core::{
    Context, Effect, EventsService, FiberState, LoggerService, Plugin, Service, async_disposer,
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
            let dispose = root.provide_str("foo", Rc::new(1i32)).unwrap();
            let timers_for_cb = timers.clone();
            let fiber = root.inject(
                &["foo"],
                Rc::new(move |_ctx, _config| {
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
            assert_eq!(fiber.state.get(), FiberState::Loading);

            dispose.dispose().await.unwrap();
            timers.advance(400).await;
            assert_eq!(fiber.state.get(), FiberState::Loading);

            timers.advance(400).await;
            assert_eq!(fiber.state.get(), FiberState::Unloading);

            drop(root.provide_str("foo", Rc::new(1i32)).unwrap());
            tokio::task::yield_now().await;
            timers.advance(1000).await;
            assert_eq!(fiber.state.get(), FiberState::Loading);

            timers.advance(1000).await;
            assert_eq!(fiber.state.get(), FiberState::Active);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_inertia_lock_2() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let dispose = root.provide_str("foo", Rc::new(1i32)).unwrap();
            let timers_for_cb = timers.clone();
            let fiber = root.inject(
                &["foo"],
                Rc::new(move |_ctx, _config| {
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
            assert_eq!(fiber.state.get(), FiberState::Loading);

            dispose.dispose().await.unwrap();
            timers.advance(400).await;
            assert_eq!(fiber.state.get(), FiberState::Loading);

            drop(root.provide_str("foo", Rc::new(2i32)).unwrap());
            timers.advance(400).await;
            assert_eq!(fiber.state.get(), FiberState::Active);
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
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|ctx, _config| {
                        drop(ctx.provide::<Foo>(Rc::new(Foo)).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            provider.wait().await.unwrap();

            let timers_for_cb = timers.clone();
            let fiber = root.inject(
                &["foo"],
                Rc::new(move |_ctx, _config| {
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
            assert_eq!(fiber.state.get(), FiberState::Loading);

            timers.advance(1000).await;
            assert_eq!(fiber.state.get(), FiberState::Active);

            provider.dispose().await;
            tokio::task::yield_now().await;
            timers.advance(2000).await;
            assert_eq!(fiber.state.get(), FiberState::Pending);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_plugin_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback_hit = Rc::new(Cell::new(0u32));
            let apply = {
                let callback_hit = callback_hit.clone();
                Rc::new(move |ctx: &Context, config: &Rc<dyn std::any::Any>| {
                    let events = ctx.get::<EventsService>().expect("events");
                    let callback_hit = callback_hit.clone();
                    drop(
                        events
                            .on(ctx, "custom", move |_| {
                                callback_hit.set(callback_hit.get() + 1);
                            })
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
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                Some(Rc::new(PluginConfig { foo: false })),
            );
            let fiber2 = root.plugin(
                &Plugin {
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                Some(Rc::new(PluginConfig { foo: true })),
            );

            tokio::task::yield_now().await;
            assert!(fiber1.wait().await.is_err());
            assert_eq!(fiber1.state.get(), FiberState::Failed);
            fiber2.wait().await.unwrap();
            assert_eq!(fiber2.state.get(), FiberState::Active);
            let logger = root.get::<LoggerService>().unwrap();
            assert_eq!(
                logger.error_count(),
                1,
                "apply error must be logged exactly once"
            );

            root.get::<EventsService>().unwrap().emit("custom", &[]);
            assert_eq!(callback_hit.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_dispose_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let dispose_called = Rc::new(Cell::new(0u32));
            let apply = {
                let dispose_called = dispose_called.clone();
                Rc::new(move |_ctx: &Context, _config: &Rc<dyn std::any::Any>| {
                    let dispose_called = dispose_called.clone();
                    Effect::Disposer(async_disposer(move || async move {
                        dispose_called.set(dispose_called.get() + 1);
                        Err::<(), Box<dyn std::error::Error>>(Box::new(std::io::Error::other(
                            "test",
                        )))
                    }))
                })
            };
            let fiber = root.plugin(
                &Plugin {
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(dispose_called.get(), 0);

            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(dispose_called.get(), 1);
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
            let calls = Rc::new(std::cell::RefCell::new(Vec::new()));
            let apply = {
                let calls = calls.clone();
                Rc::new(move |_ctx: &Context, config: &Rc<dyn std::any::Any>| {
                    let msg = config.downcast_ref::<Msg>().expect("config").msg;
                    calls.borrow_mut().push(msg);
                    Effect::None
                })
            };
            let fiber = root.plugin(
                &Plugin {
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                Some(Rc::new(Msg { msg: "hello" })),
            );
            fiber.wait().await.unwrap();
            assert_eq!(calls.borrow().len(), 1);
            assert_eq!(calls.borrow()[0], "hello");

            fiber
                .update(Some(Rc::new(Msg { msg: "world" })))
                .await
                .unwrap();
            assert_eq!(calls.borrow().len(), 2);
            assert_eq!(calls.borrow()[1], "world");

            fiber
                .update(Some(Rc::new(Msg { msg: "!!!" })))
                .await
                .unwrap();
            assert_eq!(calls.borrow().len(), 3);
            assert_eq!(calls.borrow()[2], "!!!");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_restart_wrapped_fiber() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let calls = Rc::new(Cell::new(0u32));
            let apply = {
                let calls = calls.clone();
                Rc::new(move |_ctx: &Context, _config: &Rc<dyn std::any::Any>| {
                    calls.set(calls.get() + 1);
                    Effect::None
                })
            };
            let fiber = root.plugin(
                &Plugin {
                    name: None,
                    inject: Vec::new(),
                    apply: apply.clone(),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(calls.get(), 1);

            fiber.restart().await.unwrap();
            assert_eq!(calls.get(), 2);
            assert_eq!(fiber.state.get(), FiberState::Active);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn fiber_update_config_while_injected_service_reloads() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let applied = Rc::new(std::cell::RefCell::new(Vec::new()));

            let provider_apply = Rc::new(|ctx: &Context, config: &Rc<dyn std::any::Any>| {
                let value = config
                    .downcast_ref::<ProviderConfig>()
                    .expect("config")
                    .value;
                drop(
                    ctx.provide::<Provider>(Rc::new(Provider { value }))
                        .unwrap(),
                );
                Effect::None
            });
            let provider = root.plugin(
                &Plugin {
                    name: None,
                    inject: Vec::new(),
                    apply: provider_apply,
                },
                Some(Rc::new(ProviderConfig { value: 1 })),
            );
            provider.wait().await.unwrap();

            let consumer_apply = {
                let applied = applied.clone();
                Rc::new(move |ctx: &Context, config: &Rc<dyn std::any::Any>| {
                    let mode = config
                        .downcast_ref::<ConsumerConfig>()
                        .expect("config")
                        .mode;
                    let value = ctx.get::<Provider>().expect("provider").value;
                    applied.borrow_mut().push((value, mode));
                    Effect::None
                })
            };
            let consumer = root.plugin(
                &Plugin {
                    name: None,
                    inject: vec!["provider".to_string()],
                    apply: consumer_apply,
                },
                Some(Rc::new(ConsumerConfig { mode: "old" })),
            );
            consumer.wait().await.unwrap();
            assert_eq!(applied.borrow().as_slice(), &[(1, "old")]);

            let provider_update = provider.update(Some(Rc::new(ProviderConfig { value: 2 })));
            let consumer_update = consumer.update(Some(Rc::new(ConsumerConfig { mode: "new" })));
            let (provider_result, consumer_result) = tokio::join!(provider_update, consumer_update);
            provider_result.unwrap();
            consumer_result.unwrap();

            assert_eq!(applied.borrow().as_slice(), &[(1, "old"), (2, "new")]);
            assert_eq!(consumer.state.get(), FiberState::Active);
        })
        .await;
}
