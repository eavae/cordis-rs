//! Ported cases from `packages/core/tests/service.spec.ts` and
//! `decorator.spec.ts`.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::task::{Poll, Waker};

use cordis_core::{
    Context, Effect, EventOptions, FiberState, Plugin, Service, ShadowContext, event_listener,
    service, sync_disposer,
};

/// A manually completed future used to block a service `init`.
#[derive(Clone)]
struct Gate {
    fired: Rc<Cell<bool>>,
    waker: Rc<RefCell<Option<Waker>>>,
}

impl Gate {
    fn new() -> Self {
        Self {
            fired: Rc::new(Cell::new(false)),
            waker: Rc::new(RefCell::new(None)),
        }
    }

    fn wait(&self) -> impl Future<Output = ()> {
        let gate = self.clone();
        std::future::poll_fn(move |cx| {
            *gate.waker.borrow_mut() = Some(cx.waker().clone());
            if gate.fired.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
    }

    fn fire(&self) {
        self.fired.set(true);
        if let Some(waker) = self.waker.borrow_mut().take() {
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
            let callback = Rc::new(Cell::new(0u32));
            let consumer = {
                let callback = callback.clone();
                root.inject(
                    &["foo"],
                    Rc::new(move |_ctx: &Context, _config| {
                        callback.set(callback.get() + 1);
                        Effect::None
                    }),
                )
            };
            assert_eq!(callback.get(), 0);

            // `Service.init` blocks the injector until it resolves.
            let gate = Gate::new();
            let provider = {
                let gate = gate.clone();
                root.plugin(
                    &Plugin {
                        is_group: false,
                        name: None,
                        inject: Vec::new(),
                        apply: Rc::new(move |ctx: &Context, _config| {
                            drop(ctx.provide::<Foo>(Rc::new(Foo)).unwrap());
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
            assert_eq!(callback.get(), 0, "inject blocked by Service.init");

            gate.fire();
            provider.wait().await.unwrap();
            consumer.wait().await.unwrap();
            assert_eq!(callback.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_check_gates_injector() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let available = Rc::new(Cell::new(false));
            let callback = Rc::new(Cell::new(0u32));
            let consumer = {
                let callback = callback.clone();
                root.inject(
                    &["foo"],
                    Rc::new(move |_ctx: &Context, _config| {
                        callback.set(callback.get() + 1);
                        Effect::None
                    }),
                )
            };
            let check = {
                let available = available.clone();
                Rc::new(move |_ctx: &Context| available.get()) as Rc<dyn Fn(&Context) -> bool>
            };
            let handle = root
                .provide_str_with_check("foo", Rc::new(Foo), Some(check.clone()))
                .unwrap();
            consumer.wait().await.unwrap();
            assert_eq!(callback.get(), 0, "check=false keeps the injector pending");
            assert_eq!(consumer.state.get(), FiberState::Pending);

            // Re-provide with the check now passing to trigger re-resolution.
            handle.dispose().await.unwrap();
            available.set(true);
            drop(
                root.provide_str_with_check("foo", Rc::new(Foo), Some(check))
                    .unwrap(),
            );
            consumer.wait().await.unwrap();
            assert_eq!(callback.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn service_multiple_injects() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let foo_count = Rc::new(Cell::new(0u32));
            let bar_count = Rc::new(Cell::new(0u32));
            let qux_count = Rc::new(Cell::new(0u32));

            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: vec![("qux".to_string(), None)],
                    apply: {
                        let foo_count = foo_count.clone();
                        Rc::new(move |ctx: &Context, _config| {
                            foo_count.set(foo_count.get() + 1);
                            drop(ctx.provide::<Foo>(Rc::new(Foo)).unwrap());
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
                        Rc::new(move |_ctx: &Context, _config| {
                            bar_count.set(bar_count.get() + 1);
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
                        Rc::new(move |ctx: &Context, _config| {
                            qux_count.set(qux_count.get() + 1);
                            drop(ctx.provide::<Qux>(Rc::new(Qux)).unwrap());
                            Effect::None
                        })
                    },
                },
                None,
            );
            qux_fiber.wait().await.unwrap();
            foo_fiber.wait().await.unwrap();
            bar_fiber.wait().await.unwrap();

            assert_eq!(foo_count.get(), 1);
            assert_eq!(bar_count.get(), 1);
            assert_eq!(qux_count.get(), 1);
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
            let callback = Rc::new(Cell::new(0u32));
            let dispose_called = Rc::new(Cell::new(0u32));
            let bar = Plugin {
                is_group: false,
                name: None,
                inject: vec![("foo".to_string(), None)],
                apply: {
                    let callback = callback.clone();
                    let dispose_called = dispose_called.clone();
                    Rc::new(move |_ctx: &Context, _config| {
                        callback.set(callback.get() + 1);
                        let dispose_called = dispose_called.clone();
                        Effect::Disposer(sync_disposer(move || {
                            dispose_called.set(dispose_called.get() + 1);
                        }))
                    })
                },
            };
            let bar_fiber = root.plugin(&bar, None);
            tokio::task::yield_now().await;
            assert_eq!(callback.get(), 0);
            assert_eq!(dispose_called.get(), 0);

            let foo_fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(|ctx: &Context, _config| {
                        drop(ctx.provide::<Foo>(Rc::new(Foo)).unwrap());
                        Effect::None
                    }),
                },
                None,
            );
            foo_fiber.wait().await.unwrap();
            bar_fiber.wait().await.unwrap();
            assert_eq!(callback.get(), 1);
            assert_eq!(dispose_called.get(), 0);

            foo_fiber.dispose().await;
            bar_fiber.wait().await.unwrap();
            assert_eq!(callback.get(), 1);
            assert_eq!(dispose_called.get(), 1);
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
                root.provide::<Database>(Rc::new(Database {
                    url: "cordis://local".to_string(),
                }))
                .unwrap(),
            );

            let accessor = root.database().expect("database");
            let typed = root.get::<Database>().expect("database");
            assert_eq!(accessor.url, "cordis://local");
            assert!(Rc::ptr_eq(accessor.service(), &typed));
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
            let ctx_own = root.isolate("database", Rc::from("own"));
            drop(
                ctx_own
                    .provide::<Database>(Rc::new(Database {
                        url: "cordis://own".to_string(),
                    }))
                    .unwrap(),
            );
            // The caller realm cannot see it.
            let ctx_caller = root.isolate("database", Rc::from("caller"));
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
    value: Rc<Cell<i32>>,
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
                    value.set(value.get() + 1);
                    Effect::Disposer(sync_disposer(move || {
                        value.set(value.get() - 1);
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
                root.provide::<TraceableCounter>(Rc::new(TraceableCounter {
                    value: Rc::new(Cell::new(0)),
                }))
                .unwrap(),
            );
            drop(root.provide::<TraceableFoo>(Rc::new(TraceableFoo)).unwrap());
            let value = || root.get::<TraceableCounter>().unwrap().value.get();

            // Called through the root accessor: the effect lands on the
            // root fiber and survives the inject fiber's disposal.
            root.traceable_foo().expect("foo").increase();
            assert_eq!(value(), 1);

            // Called through the inject callback's accessor: the effect
            // lands on the inject fiber.
            let fiber = root.inject(
                &["traceable_foo"],
                Rc::new(|ctx: &Context, _config| {
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
            let callback = Rc::new(Cell::new(0u32));
            let consumer = {
                let callback = callback.clone();
                root.inject(
                    &["foo"],
                    Rc::new(move |_ctx: &Context, _config| {
                        callback.set(callback.get() + 1);
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
                        apply: Rc::new(move |ctx: &Context, _config| {
                            drop(ctx.provide::<Foo>(Rc::new(Foo)).unwrap());
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
            assert_eq!(callback.get(), 0);
            root.emit("custom-event", &[]);
            provider.wait().await.unwrap();
            consumer.wait().await.unwrap();
            assert_eq!(callback.get(), 1);
        })
        .await;
}
