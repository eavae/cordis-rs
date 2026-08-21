//! Ported cases from `packages/core/tests/service.spec.ts` and
//! `decorator.spec.ts`.

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::task::{Poll, Waker};

use cordis_core::{
    Context, Effect, EventOptions, FiberState, Plugin, Service, ShadowContext, event_listener,
    service, sync_disposer,
};

/// A manually completed future used to block a service `init`.
#[derive(Clone)]
struct Gate {
    fired: Arc<AtomicBool>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            fired: Arc::new(AtomicBool::new(false)),
            waker: Arc::new(Mutex::new(None)),
        }
    }

    fn wait(&self) -> impl Future<Output = ()> {
        let gate = self.clone();
        std::future::poll_fn(move |cx| {
            *gate.waker.lock().unwrap() = Some(cx.waker().clone());
            if gate.fired.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn fire(&self) {
        self.fired.store(true, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

#[derive(Debug)]
struct Foo;

impl Service for Foo {
    const NAME: &'static str = "foo";
}

#[derive(Debug)]
struct Qux;

impl Service for Qux {
    const NAME: &'static str = "qux";
}

#[service]
struct Database {
    url: String,
}

#[tokio::test(flavor = "current_thread")]
async fn service_pending_inject() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let callback = Arc::new(AtomicU32::new(0));
            let consumer = {
                let callback = callback.clone();
                root.inject(
                    &["foo"],
                    Arc::new(move |_ctx: &Context, _config| {
                        callback.store(callback.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Effect::None
                    }),
                )
            };
            assert_eq!(callback.load(Ordering::SeqCst), 0);

            // `Service.init` blocks the injector until it resolves.
            let gate = Gate::new();
            let provider = {
                let gate = gate.clone();
                root.plugin(
                    &Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: Arc::new(move |ctx: &Context, _config| {
                            drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                            let gate = gate.clone();
                            Effect::Async(Box::pin(async move {
                                gate.wait().await;
                                Ok(sync_disposer(|| {}))
                            }))
                        }),
                    },
                    None,
                )
            };
            tokio::task::yield_now().await;
            assert_eq!(
                callback.load(Ordering::SeqCst),
                0,
                "inject blocked by Service.init"
            );

            gate.fire();
            provider.wait().await.unwrap();
            consumer.wait().await.unwrap();
            assert_eq!(callback.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_check_gates_injector() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let available = Arc::new(AtomicBool::new(false));
            let callback = Arc::new(AtomicU32::new(0));
            let consumer = {
                let callback = callback.clone();
                root.inject(
                    &["foo"],
                    Arc::new(move |_ctx: &Context, _config| {
                        callback.store(callback.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Effect::None
                    }),
                )
            };
            let check = {
                let available = available.clone();
                Arc::new(move |_ctx: &Context| available.load(Ordering::SeqCst))
                    as Arc<dyn Fn(&Context) -> bool + Send + Sync>
            };
            let handle = root
                .provide_str_with_check("foo", Arc::new(Foo), Some(check.clone()))
                .unwrap();
            consumer.wait().await.unwrap();
            assert_eq!(
                callback.load(Ordering::SeqCst),
                0,
                "check=false keeps the injector pending"
            );
            assert_eq!(consumer.state(), FiberState::Pending);

            // Re-provide with the check now passing to trigger re-resolution.
            handle.dispose().await.unwrap();
            available.store(true, Ordering::SeqCst);
            drop(
                root.provide_str_with_check("foo", Arc::new(Foo), Some(check))
                    .unwrap(),
            );
            consumer.wait().await.unwrap();
            assert_eq!(callback.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_multiple_injects() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let foo_count = Arc::new(AtomicU32::new(0));
            let bar_count = Arc::new(AtomicU32::new(0));
            let qux_count = Arc::new(AtomicU32::new(0));

            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: vec![("qux".to_string(), None)],
                    apply: {
                        let foo_count = foo_count.clone();
                        Arc::new(move |ctx: &Context, _config| {
                            foo_count.store(foo_count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                            Effect::None
                        })
                    },
                },
                None,
            );
            let bar_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: vec![("foo".to_string(), None), ("qux".to_string(), None)],
                    apply: {
                        let bar_count = bar_count.clone();
                        Arc::new(move |_ctx: &Context, _config| {
                            bar_count.store(bar_count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            Effect::None
                        })
                    },
                },
                None,
            );
            let qux_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: {
                        let qux_count = qux_count.clone();
                        Arc::new(move |ctx: &Context, _config| {
                            qux_count.store(qux_count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            drop(ctx.provide::<Qux>(Arc::new(Qux)).unwrap());
                            Effect::None
                        })
                    },
                },
                None,
            );
            qux_fiber.wait().await.unwrap();
            foo_fiber.wait().await.unwrap();
            bar_fiber.wait().await.unwrap();

            assert_eq!(foo_count.load(Ordering::SeqCst), 1);
            assert_eq!(bar_count.load(Ordering::SeqCst), 1);
            assert_eq!(qux_count.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn decorator_method_injection_equivalent() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // TS `@Inject('foo')` on a class method maps to a plugin whose
            // inject list declares `foo`; the method runs when `foo` becomes
            // available and its disposer runs when it is removed.
            let root = Context::new();
            let callback = Arc::new(AtomicU32::new(0));
            let dispose_called = Arc::new(AtomicU32::new(0));
            let bar = Plugin {
                is_group: false,
                name: None,
                inject: vec![("foo".to_string(), None)],
                apply: {
                    let callback = callback.clone();
                    let dispose_called = dispose_called.clone();
                    Arc::new(move |_ctx: &Context, _config| {
                        callback.store(callback.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        let dispose_called = dispose_called.clone();
                        Effect::Disposer(sync_disposer(move || {
                            dispose_called
                                .store(dispose_called.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }))
                    })
                },
            };
            let bar_fiber = root.plugin(&bar, None);
            tokio::task::yield_now().await;
            assert_eq!(callback.load(Ordering::SeqCst), 0);
            assert_eq!(dispose_called.load(Ordering::SeqCst), 0);

            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|ctx: &Context, _config| {
                        drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            foo_fiber.wait().await.unwrap();
            bar_fiber.wait().await.unwrap();
            assert_eq!(callback.load(Ordering::SeqCst), 1);
            assert_eq!(dispose_called.load(Ordering::SeqCst), 0);

            foo_fiber.dispose().await;
            bar_fiber.wait().await.unwrap();
            assert_eq!(callback.load(Ordering::SeqCst), 1);
            assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_macro_accessor() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::service::DatabaseServiceExt;

            let root = Context::new();
            assert!(root.database().is_none());
            drop(
                root.provide::<Database>(Arc::new(Database {
                    url: "cordis://local".to_string(),
                }))
                .unwrap(),
            );

            let accessor = root.database().expect("database");
            let typed = root.get::<Database>().expect("database");
            assert_eq!(accessor.url, "cordis://local");
            assert!(Arc::ptr_eq(accessor.service(), &typed));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_macro_accessor_uses_shadow() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::service::DatabaseServiceExt;

            let root = Context::new();
            // The database lives only in the own realm.
            let ctx_own = root.isolate("database", Arc::from("own"));
            drop(
                ctx_own
                    .provide::<Database>(Arc::new(Database {
                        url: "cordis://own".to_string(),
                    }))
                    .unwrap(),
            );
            // The caller realm cannot see it.
            let ctx_caller = root.isolate("database", Arc::from("caller"));
            assert!(ctx_caller.database().is_none());

            // The macro accessor on a ShadowContext resolves through the
            // service's own scope (JS: `this.ctx['database']`), not the
            // caller's.
            let shadow = ctx_own.shadow_of("database").expect("shadow");
            let service_ctx = ShadowContext::new(shadow.ctx, ctx_caller);
            let accessor = service_ctx.database().expect("database via own scope");
            assert_eq!(accessor.url, "cordis://own");
            // The caller chain still resolves nothing: the accessor did not
            // fall back to the caller's view.
            assert!(service_ctx.caller().database().is_none());
        })
        .await;
}

/// A counter whose `increase` registers an effect on the *traced* (caller)
/// fiber — the explicit counterpart of the TS "traceable effect" cases in
/// service.spec.ts.
#[service]
struct TraceableCounter {
    value: Arc<AtomicI32>,
}

#[service]
impl TraceableCounter {
    /// Registers an increment effect on the traced context's fiber (the
    /// caller's scope): the value increments while that scope is alive and
    /// decrements when it is disposed.
    pub fn increase(&self, ctx: &ShadowContext) {
        let value = self.value.clone();
        drop(
            ctx.effect(
                move || {
                    value.store(value.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                    Effect::Disposer(sync_disposer(move || {
                        value.store(value.load(Ordering::SeqCst) - 1, Ordering::SeqCst);
                    }))
                },
                "counter.increase",
            )
            .unwrap(),
        );
    }
}

#[service]
struct TraceableFoo;

#[service]
impl TraceableFoo {
    /// Resolves the counter through the traced context's own scope and
    /// forwards the context, so the counter's effect follows the caller.
    pub fn increase(&self, ctx: &ShadowContext) {
        ctx.traceable_counter().expect("counter").increase();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn traceable_effect_follows_call_site() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::service::TraceableFooServiceExt;

            let root = Context::new();
            drop(
                root.provide::<TraceableCounter>(Arc::new(TraceableCounter {
                    value: Arc::new(AtomicI32::new(0)),
                }))
                .unwrap(),
            );
            drop(
                root.provide::<TraceableFoo>(Arc::new(TraceableFoo))
                    .unwrap(),
            );
            let value = || {
                root.get::<TraceableCounter>()
                    .unwrap()
                    .value
                    .load(Ordering::SeqCst)
            };

            // Called through the root accessor: the effect lands on the
            // root fiber and survives the inject fiber's disposal.
            root.traceable_foo().expect("foo").increase();
            assert_eq!(value(), 1);

            // Called through the inject callback's accessor: the effect
            // lands on the inject fiber.
            let fiber = root.inject(
                &["traceable_foo"],
                Arc::new(|ctx: &Context, _config| {
                    ctx.traceable_foo().expect("foo").increase();
                    Effect::None
                }),
            );
            fiber.wait().await.unwrap();
            assert_eq!(value(), 2);

            // Disposing the inject fiber disposes only the effect registered
            // through its accessor — the traceable effect follows the call
            // site.
            fiber.dispose().await;
            assert_eq!(value(), 1);

            root.traceable_foo().expect("foo").increase();
            assert_eq!(value(), 2);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_events_during_init() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // The pending-inject spec resolves `Service.init` through an
            // event; verify the event-based wake-up path.
            let root = Context::new();
            let callback = Arc::new(AtomicU32::new(0));
            let consumer = {
                let callback = callback.clone();
                root.inject(
                    &["foo"],
                    Arc::new(move |_ctx: &Context, _config| {
                        callback.store(callback.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        Effect::None
                    }),
                )
            };
            let gate = Gate::new();
            let provider = {
                let gate = gate.clone();
                root.plugin(
                    &Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: Arc::new(move |ctx: &Context, _config| {
                            drop(ctx.provide::<Foo>(Arc::new(Foo)).unwrap());
                            let gate = gate.clone();
                            Effect::Async(Box::pin(async move {
                                gate.wait().await;
                                Ok(sync_disposer(|| {}))
                            }))
                        }),
                    },
                    None,
                )
            };
            // A listener firing the gate mirrors `ctx.on('custom-event')`
            // resolving the init promise.
            drop(
                root.on(
                    "custom-event",
                    event_listener(move |_| gate.fire()),
                    EventOptions::default(),
                )
                .unwrap(),
            );
            tokio::task::yield_now().await;
            assert_eq!(callback.load(Ordering::SeqCst), 0);
            root.emit("custom-event", &[]);
            provider.wait().await.unwrap();
            consumer.wait().await.unwrap();
            assert_eq!(callback.load(Ordering::SeqCst), 1);
        })
        .await;
}
