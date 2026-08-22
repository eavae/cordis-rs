//! Ported cases from `packages/core/tests/events.spec.ts`.

use parking_lot::Mutex;
use std::any::Any;
use std::error::Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cordis_core::{
    Context, Effect, EventCallback, EventFilter, EventOptions, ListenerFilter, Plugin,
    WaterfallNext, event_callback, event_listener, event_listener_async,
};

#[derive(Clone)]
struct Session {
    flag: bool,
}

impl EventFilter for Session {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn filter(&self, _hook_ctx: &Context) -> bool {
        true
    }
}

fn flag_filter(expected: bool) -> ListenerFilter {
    Arc::new(move |session: &dyn EventFilter| {
        session
            .as_any()
            .downcast_ref::<Session>()
            .is_none_or(|session| session.flag == expected)
    })
}

#[derive(Debug)]
struct Config {
    value: i64,
}

#[tokio::test]
async fn events_ctx_on() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    let dispose = {
        let count = count.clone();
        root.on(
            "event",
            event_listener(move |_| {
                count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
            }),
            EventOptions::default(),
        )
        .unwrap()
    };
    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    dispose.dispose().await.unwrap();
    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn events_ctx_once() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    let dispose = {
        let count = count.clone();
        root.once(
            "event",
            event_listener(move |_| {
                count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
            }),
            EventOptions::default(),
        )
        .unwrap()
    };
    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    dispose.dispose().await.unwrap();
    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn events_ctx_parallel() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| {
                count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
            }),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.parallel("event", &[], None).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.parallel("event", &[], Some(&Session { flag: false }))
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.parallel("event", &[], Some(&Session { flag: true }))
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);

    // Rejecting listeners must not short-circuit the others; errors are
    // aggregated (mirrors `AggregateError`).
    root.on(
        "event",
        event_callback(|_| Err(Box::new(std::io::Error::other("async")))),
        EventOptions::default(),
    )
    .unwrap();
    root.on(
        "event",
        event_callback(|_| Err(Box::new(std::io::Error::other("test")))),
        EventOptions::default(),
    )
    .unwrap();
    let error = root.parallel("event", &[], None).await.unwrap_err();
    assert_eq!(error.errors.len(), 2);
    let messages = error
        .errors
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(messages.contains(&"test".to_string()));
    assert!(messages.contains(&"async".to_string()));
}

#[tokio::test]
async fn events_ctx_emit() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| {
                count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
            }),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.emit("event", &[]);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.emit_with("event", &[], &Session { flag: false });
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.emit_with("event", &[], &Session { flag: true });
    assert_eq!(count.load(Ordering::SeqCst), 2);

    root.on(
        "event",
        event_callback(|_| Err(Box::new(std::io::Error::other("test")))),
        EventOptions::default(),
    )
    .unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        root.emit("event", &[]);
    }));
    assert!(
        panicked.is_err(),
        "emit must propagate the first listener error"
    );
}

#[tokio::test]
async fn events_ctx_serial() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| {
                count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
            }),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.serial("event", &[], None).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.serial("event", &[], Some(&Session { flag: false }))
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.serial("event", &[], Some(&Session { flag: true }))
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);

    root.on(
        "event",
        event_callback(|_| Err(Box::new(std::io::Error::other("message")))),
        EventOptions::default(),
    )
    .unwrap();
    assert!(root.serial("event", &[], None).await.is_err());
}

#[tokio::test]
async fn events_ctx_bail() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| {
                count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst)
            }),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.bail("event", &[], None).unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.bail("event", &[], Some(&Session { flag: false }))
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    root.bail("event", &[], Some(&Session { flag: true }))
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 2);

    root.on(
        "event",
        event_callback(|_| Err(Box::new(std::io::Error::other("message")))),
        EventOptions::default(),
    )
    .unwrap();
    assert!(root.bail("event", &[], None).is_err());
}

#[tokio::test]
async fn events_ctx_parallel_async_fan_out() {
    let root = Context::new();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    for index in 1..=2 {
        let log = log.clone();
        root.on(
            "async-event",
            event_listener_async(move |_args, _next| {
                let log = log.clone();
                async move {
                    log.lock().push(format!("start-{index}"));
                    tokio::task::yield_now().await;
                    log.lock().push(format!("end-{index}"));
                    Ok(None)
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }

    root.parallel("async-event", &[], None).await.unwrap();
    assert_eq!(
        log.lock().as_slice(),
        &["start-1", "start-2", "end-1", "end-2"],
        "all listeners must start before any continuation (concurrent fan-out)"
    );
}

#[tokio::test]
async fn events_ctx_parallel_async_aggregates_errors() {
    let root = Context::new();
    let settled = Arc::new(AtomicBool::new(false));
    {
        let settled = settled.clone();
        root.on(
            "async-errors",
            event_listener_async(move |_args, _next| {
                let settled = settled.clone();
                async move {
                    tokio::task::yield_now().await;
                    settled.store(true, Ordering::SeqCst);
                    Err(Box::<dyn Error + Send + Sync>::from(std::io::Error::other(
                        "async",
                    )))
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }
    root.on(
        "async-errors",
        event_listener_async(|_args, _next| async move {
            tokio::task::yield_now().await;
            Err(Box::<dyn Error + Send + Sync>::from(std::io::Error::other(
                "test",
            )))
        }),
        EventOptions::default(),
    )
    .unwrap();

    let error = root.parallel("async-errors", &[], None).await.unwrap_err();
    assert!(
        settled.load(Ordering::SeqCst),
        "a rejecting listener must not short-circuit the others"
    );
    assert_eq!(error.errors.len(), 2);
    let messages = error
        .errors
        .iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(messages.contains(&"async".to_string()));
    assert!(messages.contains(&"test".to_string()));
}

#[tokio::test]
async fn events_ctx_serial_async_short_circuits_in_order() {
    let root = Context::new();
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let log = log.clone();
        root.on(
            "async-serial",
            event_listener_async(move |_args, _next| {
                let log = log.clone();
                async move {
                    log.lock().push("one-start".to_string());
                    tokio::task::yield_now().await;
                    log.lock().push("one-end".to_string());
                    Ok(None)
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }
    {
        let log = log.clone();
        root.on(
            "async-serial",
            event_listener_async(move |_args, _next| {
                let log = log.clone();
                async move {
                    log.lock().push("two-start".to_string());
                    tokio::task::yield_now().await;
                    log.lock().push("two-end".to_string());
                    let value: Arc<dyn Any + Send + Sync> = Arc::new("b".to_string());
                    Ok(Some(value))
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }
    {
        let log = log.clone();
        root.on(
            "async-serial",
            event_listener_async(move |_args, _next| {
                let log = log.clone();
                async move {
                    log.lock().push("three".to_string());
                    Ok(None)
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }

    let result = root.serial("async-serial", &[], None).await.unwrap();
    assert_eq!(
        result
            .as_ref()
            .and_then(|value| value.downcast_ref::<String>())
            .map(String::as_str),
        Some("b")
    );
    assert_eq!(
        log.lock().as_slice(),
        &["one-start", "one-end", "two-start", "two-end"],
        "listeners are awaited in order and short-circuit on the first truthy result"
    );
}

#[tokio::test]
async fn events_ctx_emit_async_continues_in_background() {
    async {
        let root = Context::new();
        let done = Arc::new(AtomicBool::new(false));
        {
            let done = done.clone();
            root.on(
                "async-emit",
                event_listener_async(move |_args, _next| {
                    let done = done.clone();
                    async move {
                        tokio::task::yield_now().await;
                        done.store(true, Ordering::SeqCst);
                        Ok(None)
                    }
                }),
                EventOptions::default(),
            )
            .unwrap();
        }

        root.emit("async-emit", &[]);
        assert!(
            !done.load(Ordering::SeqCst),
            "emit must return before async listeners finish"
        );
        for _ in 0..8 {
            tokio::task::yield_now().await;
            if done.load(Ordering::SeqCst) {
                break;
            }
        }
        assert!(
            done.load(Ordering::SeqCst),
            "the background continuation must run to completion"
        );
    }
    .await;
}

#[tokio::test]
async fn events_ctx_emit_async_error_is_not_propagated() {
    async {
        let root = Context::new();
        root.on(
            "async-emit-error",
            event_listener_async(|_args, _next| async move {
                tokio::task::yield_now().await;
                Err(Box::<dyn Error + Send + Sync>::from(std::io::Error::other(
                    "late failure",
                )))
            }),
            EventOptions::default(),
        )
        .unwrap();

        root.emit("async-emit-error", &[]);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }
    .await;
}

#[tokio::test]
async fn events_ctx_bail_rejects_async_listeners() {
    let root = Context::new();
    root.on(
        "async-bail",
        event_listener_async(|_args, _next| async move {
            tokio::task::yield_now().await;
            Ok(None)
        }),
        EventOptions::default(),
    )
    .unwrap();

    let error = root.bail("async-bail", &[], None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support asynchronous listeners"),
        "bail is the synchronous short-circuit mode; async listeners must use serial"
    );
}

#[tokio::test]
async fn events_ctx_once_async() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    {
        let count = count.clone();
        root.once(
            "async-once",
            event_listener_async(move |_args, _next| {
                let count = count.clone();
                async move {
                    count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    Ok(None)
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }

    root.parallel("async-once", &[], None).await.unwrap();
    root.parallel("async-once", &[], None).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

fn waterfall_step() -> EventCallback {
    event_listener_async(|args, next| async move {
        let value = args[0].downcast_ref::<i64>().expect("value");
        let next = next.expect("next");
        let binding = next.next().await.expect("next result").expect("next value");
        let inner = binding.downcast_ref::<i64>().expect("i64");
        let result: Option<Arc<dyn Any + Send + Sync>> = Some(Arc::new(value + inner));
        Ok(result)
    })
}

fn waterfall_stop() -> EventCallback {
    event_listener_async(|args, _next| async move {
        let value = args[0].downcast_ref::<i64>().expect("value");
        let result: Option<Arc<dyn Any + Send + Sync>> = Some(Arc::new(*value));
        Ok(result)
    })
}

/// Builds a fresh one-shot waterfall tail that resolves to `value`.
fn tail_value<T: Any + Send + Sync>(value: T) -> WaterfallNext {
    WaterfallNext::new(move || {
        Box::pin(async move {
            Ok::<Option<Arc<dyn Any + Send + Sync>>, Box<dyn Error + Send + Sync>>(Some(Arc::new(
                value,
            )))
        })
    })
}

#[tokio::test]
async fn events_ctx_waterfall() {
    let root = Context::new();
    let cb1 = waterfall_step();
    let cb2 = waterfall_step();
    root.on("test/waterfall", cb1, EventOptions::default())
        .unwrap();
    root.on("test/waterfall", cb2, EventOptions::default())
        .unwrap();

    let result = root
        .waterfall("test/waterfall", &[Arc::new(1i64)], None, tail_value(2i64))
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &4);

    // A listener that does not call `next` stops the chain; earlier
    // listeners still incorporate its result.
    let cb3 = waterfall_stop();
    let cb4 = waterfall_step();
    root.on("test/waterfall", cb3, EventOptions::default())
        .unwrap();
    root.on("test/waterfall", cb4, EventOptions::default())
        .unwrap();
    let result = root
        .waterfall("test/waterfall", &[Arc::new(1i64)], None, tail_value(2i64))
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &3);
}

#[tokio::test]
async fn events_ctx_waterfall_filter() {
    let root = Context::new();
    let count = Arc::new(AtomicU32::new(0));
    {
        let count = count.clone();
        root.on_filtered(
            "test/waterfall-filter",
            event_listener_async(move |args, _next| {
                let count = count.clone();
                async move {
                    count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                    let value = args[0].downcast_ref::<i64>().expect("value");
                    let result: Option<Arc<dyn Any + Send + Sync>> = Some(Arc::new(*value + 10));
                    Ok(result)
                }
            }),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    // Unfiltered: the scoped listener receives the event.
    let result = root
        .waterfall(
            "test/waterfall-filter",
            &[Arc::new(1i64)],
            None,
            tail_value(1i64),
        )
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &11);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Filter rejects: the listener is skipped and the chain falls back to
    // the tail.
    let result = root
        .waterfall(
            "test/waterfall-filter",
            &[Arc::new(1i64)],
            Some(&Session { flag: false }),
            tail_value(1i64),
        )
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &1);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Filter accepts: the scoped listener runs again.
    let result = root
        .waterfall(
            "test/waterfall-filter",
            &[Arc::new(1i64)],
            Some(&Session { flag: true }),
            tail_value(1i64),
        )
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &11);
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn events_ctx_waterfall_async_chain() {
    let root = Context::new();
    // Listener 1 wraps the downstream result asynchronously (mirrors the
    // harness's waterfall listeners, which `await next()`).
    root.on(
        "async-waterfall",
        event_listener_async(|args, next| async move {
            let input = args[0].downcast_ref::<String>().unwrap().clone();
            let next = next.expect("next");
            let downstream = next.next().await.expect("next result").expect("next value");
            let value = downstream.downcast_ref::<String>().unwrap();
            tokio::task::yield_now().await;
            let result: Option<Arc<dyn Any + Send + Sync>> =
                Some(Arc::new(format!("[{input}] {value}")));
            Ok(result)
        }),
        EventOptions::default(),
    )
    .unwrap();
    // Listener 2 short-circuits without calling `next` for blocked input.
    root.on(
        "async-waterfall",
        event_listener_async(|args, next| async move {
            let input = args[0].downcast_ref::<String>().unwrap().clone();
            let next = next.expect("next");
            if input.contains("blocked") {
                let result: Option<Arc<dyn Any + Send + Sync>> =
                    Some(Arc::new("** blocked **".to_string()));
                Ok(result)
            } else {
                next.next().await
            }
        }),
        EventOptions::default(),
    )
    .unwrap();

    let result = root
        .waterfall(
            "async-waterfall",
            &[Arc::new("hello".to_string())],
            None,
            tail_value("hello".to_string()),
        )
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<String>().unwrap(), "[hello] hello");

    let result = root
        .waterfall(
            "async-waterfall",
            &[Arc::new("blocked words".to_string())],
            None,
            tail_value("fallback".to_string()),
        )
        .await
        .unwrap()
        .expect("result");
    assert_eq!(
        result.downcast_ref::<String>().unwrap(),
        "[blocked words] ** blocked **"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn internal_update_hook() {
    async {
        let root = Context::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let applied_seen = seen.clone();
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: Arc::new(move |_ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
                    let value = config.downcast_ref::<Config>().expect("config").value;
                    applied_seen.lock().push(("apply", value));
                    Effect::None
                }),
            },
            Some(Arc::new(Config { value: 1 })),
        );
        // Register an `internal/update` hook on the fiber's own context:
        // it runs before the default update path.
        let hook_seen = seen.clone();
        fiber
            .context()
            .on(
                "internal/update",
                event_listener_async(move |args, next| {
                    let hook_seen = hook_seen.clone();
                    async move {
                        let config = args[0].downcast_ref::<Config>().expect("config").value;
                        hook_seen.lock().push(("hook", config));
                        let next = next.expect("next");
                        let _ = next.next().await;
                        Ok(None)
                    }
                }),
                EventOptions::default(),
            )
            .unwrap();
        fiber.wait().await.unwrap();

        fiber
            .update(Some(Arc::new(Config { value: 2 })))
            .await
            .unwrap();
        assert_eq!(
            seen.lock().as_slice(),
            &[("apply", 1), ("hook", 2), ("apply", 2)]
        );
    }
    .await;
}
