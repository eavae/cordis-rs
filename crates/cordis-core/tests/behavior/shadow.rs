//! Contract tests for the explicit-parameter replacement of the TS
//! traceable/shadow/caller machinery (story cards B10/B11; gap analysis
//! `docs/test-coverage-audit.md` §3.1–3.3).
//!
//! The TS reference records `symbols.caller` (which context accessed a
//! service) and `symbols.shadow` (which context the service belongs to)
//! through a Proxy, and strips the shadow before creating plugins. The Rust
//! port passes contexts explicitly and records the shadow in the service
//! store ([`Context::shadow_of`](cordis_core::Context::shadow_of)); these
//! tests pin the behavioural contract so the two stay aligned.

use std::cell::Cell;
use std::rc::Rc;

use cordis_core::{Context, Effect, FiberState, Plugin, Service};

/// A callable service whose invoke resolves `dep` in the explicitly passed
/// context (mirrors the TS invoke.spec.ts "uses the service shadow for
/// callable extensions" case).
#[derive(Debug)]
struct Callable;

impl Service for Callable {
    const NAME: &'static str = "callable";

    fn invoke(
        &self,
        ctx: &Context,
        _init: Option<&Rc<dyn std::any::Any>>,
    ) -> Option<Rc<dyn std::any::Any>> {
        ctx.get_str("dep")
    }
}

/// A service holding the context it was constructed on. Its method forwards
/// that context to a callable's invoke — the explicit form of "service A's
/// method calls service B's invoke" from the TS specs.
#[derive(Debug)]
struct Outer {
    own_ctx: Context,
}

impl Service for Outer {
    const NAME: &'static str = "outer";
}

impl Outer {
    fn call(&self, callable: &Callable) -> Option<Rc<dyn std::any::Any>> {
        callable.invoke(&self.own_ctx, None)
    }
}

/// A service whose methods always resolve through the context bound at
/// construction — the explicit counterpart of the TS traceable `this.ctx`.
#[derive(Debug)]
struct Probe {
    own_ctx: Context,
}

impl Service for Probe {
    const NAME: &'static str = "probe";
}

impl Probe {
    fn dep(&self) -> Option<String> {
        self.own_ctx
            .get_str("dep")
            .and_then(|value| value.downcast_ref::<String>().cloned())
    }
}

/// A service that loads plugins. `load` creates the plugin on the caller's
/// context (the explicit counterpart of "strips service shadow before
/// creating plugins" in shadow.spec.ts); `load_own` is the control that
/// creates it on the loader's own context instead.
#[derive(Debug)]
struct Loader {
    own_ctx: Context,
}

impl Service for Loader {
    const NAME: &'static str = "loader";
}

impl Loader {
    fn load(&self, caller: &Context, plugin: &Plugin) -> Rc<cordis_core::Fiber> {
        caller.plugin(plugin, None)
    }

    fn load_own(&self, plugin: &Plugin) -> Rc<cordis_core::Fiber> {
        self.own_ctx.plugin(plugin, None)
    }
}

fn dep_string(value: &Rc<dyn std::any::Any>) -> &str {
    value
        .downcast_ref::<String>()
        .map(String::as_str)
        .unwrap_or("<not a string>")
}

#[tokio::test(flavor = "current_thread")]
async fn caller_scoped_invoke() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            // The callable is registered in the root scope; each scope
            // provides its own `dep`.
            drop(root.provide::<Callable>(Rc::new(Callable)).unwrap());
            drop(
                root.provide_str("dep", Rc::new("root".to_string()))
                    .unwrap(),
            );
            let ctx_a = root.isolate("dep", Rc::from("scope-a"));
            drop(ctx_a.provide_str("dep", Rc::new("a".to_string())).unwrap());
            let ctx_b = root.isolate("dep", Rc::from("scope-b"));
            drop(ctx_b.provide_str("dep", Rc::new("b".to_string())).unwrap());

            let callable = root.get::<Callable>().expect("callable");

            // Service A's method invokes the callable with A's own context:
            // the dependency resolves in A's scope, not in the callable's
            // registration scope (audit §3.2).
            drop(
                ctx_a
                    .provide::<Outer>(Rc::new(Outer {
                        own_ctx: ctx_a.clone(),
                    }))
                    .unwrap(),
            );
            let outer = root.get::<Outer>().expect("outer");
            let result = outer.call(&callable).expect("callable must resolve dep");
            assert_eq!(dep_string(&result), "a");

            // The same callable invoked from another caller scope follows
            // that caller's scope instead.
            let result = ctx_b.invoke::<Callable>(None).expect("callable");
            assert_eq!(dep_string(&result), "b");

            // And invoked from its own registration scope, it resolves
            // there — caller and shadow stay separate, as in JS.
            let result = root.invoke::<Callable>(None).expect("callable");
            assert_eq!(dep_string(&result), "root");

            let shadow = root.shadow_of("callable").expect("shadow");
            assert_eq!(shadow.name, "callable");
            assert!(shadow.ctx.shares_inner(&root), "callable belongs to root");
            assert!(
                !shadow.ctx.shares_inner(&ctx_a),
                "callable's shadow must not be the caller scope"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bound_ctx_contract() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            drop(
                root.provide_str("dep", Rc::new("root".to_string()))
                    .unwrap(),
            );

            let ctx_own = root.isolate("dep", Rc::from("own"));
            drop(
                ctx_own
                    .provide_str("dep", Rc::new("own".to_string()))
                    .unwrap(),
            );
            drop(
                ctx_own
                    .provide::<Probe>(Rc::new(Probe {
                        own_ctx: ctx_own.clone(),
                    }))
                    .unwrap(),
            );

            // The method is invoked from `root` — a different context — yet
            // resolves through the construction-bound context: no traceable
            // caller switching (audit §3.3 reverse contract).
            let probe = root.get::<Probe>().expect("probe");
            assert_eq!(probe.dep().as_deref(), Some("own"));

            // The recorded shadow matches the construction context.
            let shadow = root.shadow_of("probe").expect("shadow");
            assert_eq!(shadow.name, "probe");
            assert!(shadow.ctx.shares_inner(&ctx_own));
            assert!(!shadow.ctx.shares_inner(&root));
            assert_eq!(
                shadow
                    .ctx
                    .get_str("dep")
                    .and_then(|value| value.downcast_ref::<String>().cloned())
                    .as_deref(),
                Some("own")
            );
            // Isolate layers share the parent fiber, so the shadow's fiber
            // is the provider fiber (root's here).
            assert!(Rc::ptr_eq(shadow.ctx.fiber(), root.fiber()));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn caller_scoped_plugin() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            #[derive(Debug)]
            struct Server;
            drop(root.provide_str("server", Rc::new(Server)).unwrap());

            // The loader lives in a scope that cannot see `server`; only the
            // caller's scope can.
            let loader_scope = root.isolate("server", Rc::from("loader"));
            drop(
                loader_scope
                    .provide::<Loader>(Rc::new(Loader {
                        own_ctx: loader_scope.clone(),
                    }))
                    .unwrap(),
            );

            let applied = Rc::new(Cell::new(0u32));
            let resolved = Rc::new(Cell::new(false));
            let consumer = {
                let applied = applied.clone();
                let resolved = resolved.clone();
                Plugin {
                    is_group: false,
                    name: None,
                    inject: vec![("server".to_string(), None)],
                    apply: Rc::new(move |ctx: &Context, _config| {
                        applied.set(applied.get() + 1);
                        // Mirror the `server instanceof Server` assertion in
                        // shadow.spec.ts: the resolved value must be the
                        // caller scope's Server instance.
                        if let Some(value) = ctx.get_str("server") {
                            resolved.set(value.downcast_ref::<Server>().is_some());
                        }
                        Effect::None
                    }),
                }
            };

            let loader = root.get::<Loader>().expect("loader");

            // Creating the plugin through the caller's context resolves its
            // injects in the caller's scope.
            let fiber = loader.load(&root, &consumer);
            fiber.wait().await.unwrap();
            assert_eq!(applied.get(), 1);
            assert!(resolved.get(), "server must resolve in the caller scope");

            // Control: creating the plugin through the loader's own context
            // (the "un-stripped shadow" case in JS) leaves the inject
            // unresolved.
            let fiber = loader.load_own(&consumer);
            assert_eq!(fiber.state.get(), FiberState::Pending);
            assert_eq!(applied.get(), 1);

            // The recorded shadow says where the loader itself belongs.
            let shadow = root.shadow_of("loader").expect("shadow");
            assert!(shadow.ctx.shares_inner(&loader_scope));
        })
        .await;
}
