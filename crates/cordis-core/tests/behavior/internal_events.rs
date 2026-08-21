//! The `internal/*` event matrix.
//!
//! Covers `internal/dispatch`, `internal/get`, `internal/set`,
//! `internal/service` and `internal/status`, mirroring the TS reference
//! (`events.ts` `_resolve`, `reflect.ts` waterfall/provide paths and
//! `fiber.ts` `_updateState`).

use std::any::Any;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use cordis_core::{
    Context, Effect, EventCallback, EventOptions, FiberState, Plugin, WaterfallNext,
    event_callback, event_listener_async,
};

/// Records `(mode, name, arg_count)` for `internal/dispatch` payloads.
fn dispatch_recorder(records: Arc<Mutex<Vec<(String, String, usize)>>>) -> EventCallback {
    event_callback(move |args: &[Arc<dyn Any + Send + Sync>]| {
        let mode = args[0].downcast_ref::<String>().unwrap().clone();
        let name = args[1].downcast_ref::<String>().unwrap().clone();
        let payload = args[2]
            .downcast_ref::<Vec<Arc<dyn Any + Send + Sync>>>()
            .unwrap();
        records.lock().unwrap().push((mode, name, payload.len()));
        Ok(None)
    })
}

/// `internal/dispatch` fires before non-internal events only, with the
/// dispatch mode and payload args.
#[test]
fn internal_dispatch_hook_fires_for_external_events_only() {
    let root = Context::new();
    let records = Arc::new(Mutex::new(Vec::new()));
    drop(
        root.on(
            "internal/dispatch",
            dispatch_recorder(records.clone()),
            EventOptions::default(),
        )
        .unwrap(),
    );

    root.emit("demo", &[Arc::new(1u32), Arc::new("x".to_string())]);
    let _ = root.bail("demo2", &[Arc::new(2u32)], None);
    // Internal events must not re-enter the dispatch hook.
    root.emit("internal/update", &[Arc::new(())]);

    let records = records.lock().unwrap();
    assert_eq!(
        records.as_slice(),
        &[
            ("emit".to_string(), "demo".to_string(), 2),
            ("bail".to_string(), "demo2".to_string(), 1),
        ]
    );
}

/// `internal/get` is a waterfall over dynamic access — a hook can override
/// the value (MockLoader-style, e.g. allowing the root context to read
/// `loader`) or fall through to the strict store lookup via `next()`.
#[test]
fn internal_get_hook_overrides_dynamic_access() {
    let root = Context::new();
    assert!(root.get_str("loader").is_none(), "no loader without a hook");

    let get_hook: EventCallback = event_listener_async(|args| async move {
        let name = args[1].downcast_ref::<String>().unwrap().clone();
        let next = args[3].clone().downcast::<WaterfallNext>().unwrap();
        match name.as_str() {
            "loader" => {
                let value: Arc<dyn Any + Send + Sync> = Arc::new("mock loader".to_string());
                Ok(Some(value))
            }
            "foo" => {
                let value: Arc<dyn Any + Send + Sync> = Arc::new("overridden".to_string());
                Ok(Some(value))
            }
            _ => next.next().await,
        }
    });
    drop(
        root.on("internal/get", get_hook, EventOptions::default())
            .unwrap(),
    );

    // Hook short-circuits a missing service (MockLoader-style root access).
    let loader = root.get_str("loader").expect("hook must provide loader");
    assert_eq!(
        loader.downcast_ref::<String>().unwrap(),
        "mock loader",
        "hook must override the value"
    );

    // Hook overrides a real service value.
    drop(root.provide_str("foo", Arc::new(1u32)).unwrap());
    let foo = root.get_str("foo").expect("foo is provided");
    assert_eq!(
        foo.downcast_ref::<String>().unwrap(),
        "overridden",
        "hook must override an existing service"
    );

    // Falling through with next() keeps the strict lookup behavior.
    assert!(
        root.get_str("nope").is_none(),
        "next() falls back to lookup"
    );
}

/// `internal/set` is a waterfall over dynamic writes — a hook can accept
/// the write without touching the store, reject it, or fall through to the
/// strict store update via `next()`.
#[test]
fn internal_set_hook_intercepts_write() {
    let root = Context::new();
    drop(root.provide_str("foo", Arc::new(1u32)).unwrap());

    let intercept = Arc::new(AtomicBool::new(false));
    let hook_intercept = intercept.clone();
    let set_hook: EventCallback = event_listener_async(move |args| {
        let intercept = hook_intercept.clone();
        async move {
            let name = args[1].downcast_ref::<String>().unwrap().clone();
            let next = args[4].clone().downcast::<WaterfallNext>().unwrap();
            match name.as_str() {
                "foo" if intercept.load(Ordering::SeqCst) => {
                    let value: Arc<dyn Any + Send + Sync> = Arc::new(true);
                    Ok(Some(value))
                }
                "bar" => {
                    let value: Arc<dyn Any + Send + Sync> = Arc::new(false);
                    Ok(Some(value))
                }
                _ => next.next().await,
            }
        }
    });
    drop(
        root.on("internal/set", set_hook, EventOptions::default())
            .unwrap(),
    );

    // Hook accepts the write without calling next(): the store is untouched.
    intercept.store(true, Ordering::SeqCst);
    root.set_str("foo", Arc::new(2u32))
        .expect("intercepted write");
    let foo = root.get_str("foo").unwrap();
    assert_eq!(
        foo.downcast_ref::<u32>().copied(),
        Some(1),
        "accepted write must not reach the store"
    );

    // Falling through with next() updates the store.
    intercept.store(false, Ordering::SeqCst);
    root.set_str("foo", Arc::new(2u32))
        .expect("fallthrough write");
    let foo = root.get_str("foo").unwrap();
    assert_eq!(foo.downcast_ref::<u32>().copied(), Some(2));

    // A hook rejecting the write makes set_str fail.
    assert!(
        root.set_str("bar", Arc::new(3u32)).is_err(),
        "rejected write must fail"
    );
}

/// `internal/service` is a filter-directed broadcast on provide — only
/// listeners whose isolate label for the name matches the provider's realm
/// receive it (mirrors the "isolated event" semantics).
#[tokio::test(flavor = "current_thread")]
async fn internal_service_broadcasts_to_same_realm() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let ctx = root.isolate("foo", Arc::from("shared-label"));

            let root_seen = Arc::new(Mutex::new(Vec::new()));
            let ctx_seen = Arc::new(Mutex::new(Vec::new()));
            drop(
                root.on(
                    "internal/service",
                    service_recorder(root_seen.clone()),
                    EventOptions::default(),
                )
                .unwrap(),
            );
            drop(
                ctx.on(
                    "internal/service",
                    service_recorder(ctx_seen.clone()),
                    EventOptions::default(),
                )
                .unwrap(),
            );

            // Provided on the root realm: only the root listener sees it.
            let root_provide = root
                .provide_str("foo", Arc::new("root foo".to_string()))
                .unwrap();
            assert_eq!(
                root_seen.lock().unwrap().as_slice(),
                &["root foo".to_string()]
            );
            assert!(
                ctx_seen.lock().unwrap().is_empty(),
                "different realm must not see it"
            );

            // Provided on the isolated realm: only the same-realm listener
            // sees it.
            let ctx_provide = ctx
                .provide_str("foo", Arc::new("isolated foo".to_string()))
                .unwrap();
            assert_eq!(
                ctx_seen.lock().unwrap().as_slice(),
                &["isolated foo".to_string()]
            );
            assert_eq!(
                root_seen.lock().unwrap().as_slice(),
                &["root foo".to_string()],
                "root listener must not receive the isolated provide"
            );

            drop(root_provide);
            drop(ctx_provide);
        })
        .await;
}

fn service_recorder(records: Arc<Mutex<Vec<String>>>) -> EventCallback {
    event_callback(move |args: &[Arc<dyn Any + Send + Sync>]| {
        let name = args[0].downcast_ref::<String>().unwrap();
        if name == "foo"
            && let Some(value) = args[1].downcast_ref::<String>()
        {
            records.lock().unwrap().push(value.clone());
        }
        Ok(None)
    })
}

/// `internal/status` broadcasts every fiber state transition with the fiber
/// and its previous state (mirrors fiber.ts `_updateState`).
#[tokio::test(flavor = "current_thread")]
async fn internal_status_broadcasts_transitions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let records = Arc::new(Mutex::new(Vec::new()));
            let records_for_hook = records.clone();
            let status_hook: EventCallback =
                event_callback(move |args: &[Arc<dyn Any + Send + Sync>]| {
                    // `Arc<dyn Any + Send + Sync>` erases the inner type, so the fiber arrives
                    // as `&Fiber` (mirrors the loader's internal/plugin hooks).
                    let fiber = args[0].downcast_ref::<cordis_core::Fiber>().unwrap();
                    let old = args[1].downcast_ref::<FiberState>().copied().unwrap();
                    records_for_hook
                        .lock()
                        .unwrap()
                        .push((fiber as *const cordis_core::Fiber as usize, old));
                    Ok(None)
                });
            drop(
                root.on("internal/status", status_hook, EventOptions::default())
                    .unwrap(),
            );
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(|_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();

            let seen = records.lock().unwrap().clone();
            assert!(
                seen.iter().any(|(ptr, old)| {
                    *ptr == Arc::as_ptr(&fiber) as usize && *old == FiberState::Pending
                }),
                "must broadcast Pending → Loading with the fiber: {seen:?}"
            );
            assert!(
                seen.iter().any(|(ptr, old)| {
                    *ptr == Arc::as_ptr(&fiber) as usize && *old == FiberState::Loading
                }),
                "must broadcast Loading → Active: {seen:?}"
            );

            let _ = tokio::task::spawn_local(fiber.dispose()).await;
            let seen = records.lock().unwrap().clone();
            assert!(
                seen.iter().any(|(ptr, old)| {
                    *ptr == Arc::as_ptr(&fiber) as usize && *old == FiberState::Active
                }),
                "must broadcast Active → Unloading on dispose: {seen:?}"
            );
            assert!(
                seen.iter().any(|(ptr, old)| {
                    *ptr == Arc::as_ptr(&fiber) as usize && *old == FiberState::Unloading
                }),
                "must broadcast Unloading → Disposed: {seen:?}"
            );
        })
        .await;
}
