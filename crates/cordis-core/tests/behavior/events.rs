//! Ported cases from `packages/core/tests/events.spec.ts`.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::rc::Rc;

use cordis_core::{
    AnyNext, Context, Effect, EventCallback, EventFilter, EventOptions, ListenerFilter, Plugin,
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
    Rc::new(move |session: &dyn EventFilter| {
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
    let count = Rc::new(Cell::new(0u32));
    let dispose = {
        let count = count.clone();
        root.on(
            "event",
            event_listener(move |_| count.set(count.get() + 1)),
            EventOptions::default(),
        )
        .unwrap()
    };
    root.emit("event", &[]);
    assert_eq!(count.get(), 1);
    root.emit("event", &[]);
    assert_eq!(count.get(), 2);
    dispose.dispose().await.unwrap();
    root.emit("event", &[]);
    assert_eq!(count.get(), 2);
}

#[tokio::test]
async fn events_ctx_once() {
    let root = Context::new();
    let count = Rc::new(Cell::new(0u32));
    let dispose = {
        let count = count.clone();
        root.once(
            "event",
            event_listener(move |_| count.set(count.get() + 1)),
            EventOptions::default(),
        )
        .unwrap()
    };
    root.emit("event", &[]);
    assert_eq!(count.get(), 1);
    root.emit("event", &[]);
    assert_eq!(count.get(), 1);
    dispose.dispose().await.unwrap();
    root.emit("event", &[]);
    assert_eq!(count.get(), 1);
}

#[tokio::test]
async fn events_ctx_parallel() {
    let root = Context::new();
    let count = Rc::new(Cell::new(0u32));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| count.set(count.get() + 1)),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.parallel("event", &[], None).await.unwrap();
    assert_eq!(count.get(), 1);
    root.parallel("event", &[], Some(&Session { flag: false }))
        .await
        .unwrap();
    assert_eq!(count.get(), 1);
    root.parallel("event", &[], Some(&Session { flag: true }))
        .await
        .unwrap();
    assert_eq!(count.get(), 2);

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
    let count = Rc::new(Cell::new(0u32));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| count.set(count.get() + 1)),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.emit("event", &[]);
    assert_eq!(count.get(), 1);
    root.emit_with("event", &[], &Session { flag: false });
    assert_eq!(count.get(), 1);
    root.emit_with("event", &[], &Session { flag: true });
    assert_eq!(count.get(), 2);

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
    let count = Rc::new(Cell::new(0u32));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| count.set(count.get() + 1)),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.serial("event", &[], None).await.unwrap();
    assert_eq!(count.get(), 1);
    root.serial("event", &[], Some(&Session { flag: false }))
        .await
        .unwrap();
    assert_eq!(count.get(), 1);
    root.serial("event", &[], Some(&Session { flag: true }))
        .await
        .unwrap();
    assert_eq!(count.get(), 2);

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
    let count = Rc::new(Cell::new(0u32));
    {
        let count = count.clone();
        root.on_filtered(
            "event",
            event_listener(move |_| count.set(count.get() + 1)),
            EventOptions::default(),
            flag_filter(true),
        )
        .unwrap();
    }

    root.bail("event", &[], None).unwrap();
    assert_eq!(count.get(), 1);
    root.bail("event", &[], Some(&Session { flag: false }))
        .unwrap();
    assert_eq!(count.get(), 1);
    root.bail("event", &[], Some(&Session { flag: true }))
        .unwrap();
    assert_eq!(count.get(), 2);

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
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    for index in 1..=2 {
        let log = log.clone();
        root.on(
            "async-event",
            event_listener_async(move |_args| {
                let log = log.clone();
                async move {
                    log.borrow_mut().push(format!("start-{index}"));
                    tokio::task::yield_now().await;
                    log.borrow_mut().push(format!("end-{index}"));
                    Ok(None)
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }

    root.parallel("async-event", &[], None).await.unwrap();
    assert_eq!(
        log.borrow().as_slice(),
        &["start-1", "start-2", "end-1", "end-2"],
        "all listeners must start before any continuation (concurrent fan-out)"
    );
}

#[tokio::test]
async fn events_ctx_parallel_async_aggregates_errors() {
    let root = Context::new();
    let settled = Rc::new(Cell::new(false));
    {
        let settled = settled.clone();
        root.on(
            "async-errors",
            event_listener_async(move |_args| {
                let settled = settled.clone();
                async move {
                    tokio::task::yield_now().await;
                    settled.set(true);
                    Err(Box::<dyn Error>::from(std::io::Error::other("async")))
                }
            }),
            EventOptions::default(),
        )
        .unwrap();
    }
    root.on(
        "async-errors",
        event_listener_async(|_args| async move {
            tokio::task::yield_now().await;
            Err(Box::<dyn Error>::from(std::io::Error::other("test")))
        }),
        EventOptions::default(),
    )
    .unwrap();

    let error = root.parallel("async-errors", &[], None).await.unwrap_err();
    assert!(
        settled.get(),
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
    let log = Rc::new(RefCell::new(Vec::<String>::new()));
    {
        let log = log.clone();
        root.on(
            "async-serial",
            event_listener_async(move |_args| {
                let log = log.clone();
                async move {
                    log.borrow_mut().push("one-start".to_string());
                    tokio::task::yield_now().await;
                    log.borrow_mut().push("one-end".to_string());
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
            event_listener_async(move |_args| {
                let log = log.clone();
                async move {
                    log.borrow_mut().push("two-start".to_string());
                    tokio::task::yield_now().await;
                    log.borrow_mut().push("two-end".to_string());
                    let value: Rc<dyn Any> = Rc::new("b".to_string());
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
            event_listener_async(move |_args| {
                let log = log.clone();
                async move {
                    log.borrow_mut().push("three".to_string());
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
        log.borrow().as_slice(),
        &["one-start", "one-end", "two-start", "two-end"],
        "listeners are awaited in order and short-circuit on the first truthy result"
    );
}

#[tokio::test]
async fn events_ctx_emit_async_continues_in_background() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let done = Rc::new(Cell::new(false));
            {
                let done = done.clone();
                root.on(
                    "async-emit",
                    event_listener_async(move |_args| {
                        let done = done.clone();
                        async move {
                            tokio::task::yield_now().await;
                            done.set(true);
                            Ok(None)
                        }
                    }),
                    EventOptions::default(),
                )
                .unwrap();
            }

            root.emit("async-emit", &[]);
            assert!(
                !done.get(),
                "emit must return before async listeners finish"
            );
            for _ in 0..8 {
                tokio::task::yield_now().await;
                if done.get() {
                    break;
                }
            }
            assert!(
                done.get(),
                "the background continuation must run to completion"
            );
        })
        .await;
}

#[tokio::test]
async fn events_ctx_emit_async_error_is_not_propagated() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            root.on(
                "async-emit-error",
                event_listener_async(|_args| async move {
                    tokio::task::yield_now().await;
                    Err(Box::<dyn Error>::from(std::io::Error::other(
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
        })
        .await;
}

#[tokio::test]
async fn events_ctx_bail_rejects_async_listeners() {
    let root = Context::new();
    root.on(
        "async-bail",
        event_listener_async(|_args| async move {
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
    let count = Rc::new(Cell::new(0u32));
    {
        let count = count.clone();
        root.once(
            "async-once",
            event_listener_async(move |_args| {
                let count = count.clone();
                async move {
                    count.set(count.get() + 1);
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
    assert_eq!(count.get(), 1);
}

fn waterfall_step() -> EventCallback {
    event_listener_async(|args| async move {
        let value = args[0].downcast_ref::<i64>().expect("value");
        let next = args[1].downcast_ref::<AnyNext>().expect("next").0.clone();
        let binding = next().await.expect("next result").expect("next value");
        let inner = binding.downcast_ref::<i64>().expect("i64");
        let result: Option<Rc<dyn Any>> = Some(Rc::new(value + inner));
        Ok(result)
    })
}

fn waterfall_stop() -> EventCallback {
    event_listener_async(|args| async move {
        let value = args[0].downcast_ref::<i64>().expect("value");
        let result: Option<Rc<dyn Any>> = Some(Rc::new(*value));
        Ok(result)
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
        .waterfall(
            "test/waterfall",
            &[Rc::new(1i64)],
            Rc::new(|| {
                Box::pin(async { Ok::<Option<Rc<dyn Any>>, Box<dyn Error>>(Some(Rc::new(2i64))) })
            }),
        )
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
        .waterfall(
            "test/waterfall",
            &[Rc::new(1i64)],
            Rc::new(|| {
                Box::pin(async { Ok::<Option<Rc<dyn Any>>, Box<dyn Error>>(Some(Rc::new(2i64))) })
            }),
        )
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &3);
}

#[tokio::test]
async fn events_ctx_waterfall_async_chain() {
    let root = Context::new();
    // Listener 1 wraps the downstream result asynchronously (mirrors the
    // harness's waterfall listeners, which `await next()`).
    root.on(
        "async-waterfall",
        event_listener_async(|args| async move {
            let input = args[0].downcast_ref::<String>().unwrap().clone();
            let next = args[1].downcast_ref::<AnyNext>().unwrap().0.clone();
            let downstream = next().await.expect("next result").expect("next value");
            let value = downstream.downcast_ref::<String>().unwrap();
            tokio::task::yield_now().await;
            let result: Option<Rc<dyn Any>> = Some(Rc::new(format!("[{input}] {value}")));
            Ok(result)
        }),
        EventOptions::default(),
    )
    .unwrap();
    // Listener 2 short-circuits without calling `next` for blocked input.
    root.on(
        "async-waterfall",
        event_listener_async(|args| async move {
            let input = args[0].downcast_ref::<String>().unwrap().clone();
            let next = args[1].downcast_ref::<AnyNext>().unwrap().0.clone();
            if input.contains("blocked") {
                let result: Option<Rc<dyn Any>> = Some(Rc::new("** blocked **".to_string()));
                Ok(result)
            } else {
                next().await
            }
        }),
        EventOptions::default(),
    )
    .unwrap();

    let tail: WaterfallNext = Rc::new(|| {
        Box::pin(async {
            Ok::<Option<Rc<dyn Any>>, Box<dyn Error>>(Some(Rc::new("hello".to_string())))
        })
    });
    let result = root
        .waterfall("async-waterfall", &[Rc::new("hello".to_string())], tail)
        .await
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<String>().unwrap(), "[hello] hello");

    let tail: WaterfallNext = Rc::new(|| {
        Box::pin(async {
            Ok::<Option<Rc<dyn Any>>, Box<dyn Error>>(Some(Rc::new("fallback".to_string())))
        })
    });
    let result = root
        .waterfall(
            "async-waterfall",
            &[Rc::new("blocked words".to_string())],
            tail,
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
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let seen = Rc::new(RefCell::new(Vec::new()));
            let applied_seen = seen.clone();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(move |_ctx: &Context, config: &Rc<dyn Any>| {
                        let value = config.downcast_ref::<Config>().expect("config").value;
                        applied_seen.borrow_mut().push(("apply", value));
                        Effect::None
                    }),
                },
                Some(Rc::new(Config { value: 1 })),
            );
            // Register an `internal/update` hook on the fiber's own context:
            // it runs before the default update path.
            let hook_seen = seen.clone();
            fiber
                .context()
                .on(
                    "internal/update",
                    event_listener_async(move |args| {
                        let hook_seen = hook_seen.clone();
                        async move {
                            let config = args[0].downcast_ref::<Config>().expect("config").value;
                            hook_seen.borrow_mut().push(("hook", config));
                            let next = args[3].downcast_ref::<AnyNext>().expect("next").0.clone();
                            let _ = next().await;
                            Ok(None)
                        }
                    }),
                    EventOptions::default(),
                )
                .unwrap();
            fiber.wait().await.unwrap();

            fiber
                .update(Some(Rc::new(Config { value: 2 })))
                .await
                .unwrap();
            assert_eq!(
                seen.borrow().as_slice(),
                &[("apply", 1), ("hook", 2), ("apply", 2)]
            );
        })
        .await;
}
