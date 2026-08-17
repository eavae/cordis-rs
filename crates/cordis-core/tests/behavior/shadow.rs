//! Contract tests for the traceable/shadow/caller machinery.
//!
//! The TS reference hands service methods a hybrid `this.ctx`: dependency
//! reads resolve through the service's own shadow, while intercept / fiber /
//! plugin / effect reads follow the caller's chain. The Rust port models the
//! same split with [`ShadowContext`](cordis_core::ShadowContext), and the
//! `#[service]` accessor restores the automatic tracing at the access point:
//! `ctx.foo()` returns a handle whose forwarded methods inject the traced
//! context. These tests pin the behavioural contract so the two stay
//! aligned.

use std::cell::Cell;
use std::rc::Rc;

use cordis_core::{Context, Effect, FiberState, Plugin, Service, ShadowContext, service};

/// A callable service whose invoke resolves `dep` through the service's own
/// shadow (mirrors the TS invoke.spec.ts "uses the service shadow for
/// callable extensions" case). Callables stay manual: the framework builds
/// the context in [`Context::invoke_str`].
#[derive(Debug)]
struct Callable;

impl Service for Callable {
    const NAME: &'static str = "callable";

    fn invoke(
        &self,
        ctx: &ShadowContext,
        _init: Option<&Rc<dyn std::any::Any>>,
    ) -> Option<Rc<dyn std::any::Any>> {
        // `get_str` routes to `own` — the callable's own realm, not the
        // caller's (JS: `this.ctx['dependency']` → the service shadow).
        ctx.get_str("dep")
    }
}

/// A service whose method invokes a callable through its traced context —
/// the explicit form of "service A's method calls service B's invoke" from
/// the TS specs. `ctx.invoke` resolves the callable through `own` and keeps
/// the caller chain, exactly like `this.ctx['callable']()` in JS.
#[service]
struct Outer;

#[service]
impl Outer {
    pub fn call(&self, ctx: &ShadowContext) -> Option<Rc<dyn std::any::Any>> {
        ctx.invoke::<Callable>(None)
    }
}

/// A service whose methods resolve dependencies through the context recorded
/// as its shadow — the explicit counterpart of the TS traceable `this.ctx`.
#[service]
struct Probe;

#[service]
impl Probe {
    pub fn dep(&self, ctx: &ShadowContext) -> Option<String> {
        ctx.get_str("dep")
            .and_then(|value| value.downcast_ref::<String>().cloned())
    }
}

/// A service that loads plugins. `load` creates the plugin on the caller's
/// context (the explicit counterpart of "strips service shadow before
/// creating plugins" in shadow.spec.ts); `load_own` is the control that
/// creates it on the loader's own context instead.
#[service]
struct Loader {
    own_ctx: Context,
}

#[service]
impl Loader {
    pub fn load(&self, ctx: &ShadowContext, plugin: &Plugin) -> Rc<cordis_core::Fiber> {
        // `plugin` derefs to the caller's context (JS: `this.ctx.plugin`
        // runs on the stripped caller ctx).
        ctx.plugin(plugin, None)
    }

    pub fn load_own(&self, plugin: &Plugin) -> Rc<cordis_core::Fiber> {
        self.own_ctx.plugin(plugin, None)
    }
}

fn dep_string(value: &Rc<dyn std::any::Any>) -> &str {
    value
        .downcast_ref::<String>()
        .map(String::as_str)
        .unwrap_or("<not a string>")
}

fn dep_owned(value: &Rc<dyn std::any::Any>) -> String {
    dep_string(value).to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn invoke_uses_service_shadow() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::shadow::OuterServiceExt;

            let root = Context::new();
            drop(
                root.provide_str("dep", Rc::new("root".to_string()))
                    .unwrap(),
            );

            // The callable lives in realm X, where `dep` = "x"; the callers
            // live in root / realm Y with their own `dep` values.
            let realm_x = root.isolate("dep", Rc::from("scope-x"));
            drop(
                realm_x
                    .provide_str("dep", Rc::new("x".to_string()))
                    .unwrap(),
            );
            drop(realm_x.provide::<Callable>(Rc::new(Callable)).unwrap());
            let realm_y = root.isolate("dep", Rc::from("scope-y"));
            drop(
                realm_y
                    .provide_str("dep", Rc::new("y".to_string()))
                    .unwrap(),
            );
            drop(realm_y.provide::<Outer>(Rc::new(Outer)).unwrap());

            // Framework entry: the callable's own shadow drives the DI
            // resolution regardless of the invoking context.
            let result = realm_y.invoke::<Callable>(None).expect("callable");
            assert_eq!(dep_string(&result), "x");
            let result = root.invoke::<Callable>(None).expect("callable");
            assert_eq!(dep_string(&result), "x");

            // The recorded shadow points at the callable's realm, not the
            // caller's.
            let shadow = root.shadow_of("callable").expect("shadow");
            assert_eq!(shadow.name, "callable");
            assert_eq!(
                shadow
                    .ctx
                    .get_str("dep")
                    .map(|value| dep_owned(&value))
                    .unwrap_or_else(|| "<none>".to_string()),
                "x"
            );
            assert!(
                !shadow.ctx.shares_inner(&realm_y),
                "callable's shadow must not be the caller realm"
            );

            // Service A's method invoked through the traced handle: the
            // accessor built the context (own = Outer's shadow, caller =
            // root), and `ctx.invoke` resolves the callable through own.
            let outer = root.outer().expect("outer");
            let result = outer.call().expect("callable must resolve dep");
            assert_eq!(dep_string(&result), "x");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn bound_ctx_contract() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::shadow::ProbeServiceExt;

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
            drop(ctx_own.provide::<Probe>(Rc::new(Probe)).unwrap());

            // The recorded shadow is the construction context.
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

            // Called through the traced handle from `root`: the accessor
            // built own from the recorded shadow, so the method resolves in
            // its own realm (audit §3.3 reverse contract: DI never follows
            // the call site).
            let result = root.probe().expect("probe").dep();
            assert_eq!(result.as_deref(), Some("own"));

            // Explicit escape hatch: with a different `own`, the same method
            // resolves in that scope — `own` is explicit, never inferred.
            let bare = root.get::<Probe>().expect("probe");
            let result = bare.dep(&ShadowContext::new(root.clone(), root.clone()));
            assert_eq!(result.as_deref(), Some("root"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn caller_scoped_plugin() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            use crate::behavior::shadow::LoaderServiceExt;

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

            // Through the traced handle: `load` creates the plugin on the
            // caller's context (the handle's caller chain = root), not the
            // loader's own (`own`).
            let loader = root.loader().expect("loader");
            let fiber = loader.load(&consumer);
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
