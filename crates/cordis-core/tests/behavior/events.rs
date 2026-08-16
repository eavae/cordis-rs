//! Ported cases from `packages/core/tests/events.spec.ts` (story card B5).

use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;

use cordis_core::{
    AnyNext, Context, Effect, EventCallback, EventFilter, EventOptions, ListenerFilter, Plugin,
    event_listener,
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
            .map(|session| session.flag == expected)
            .unwrap_or(true)
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
        Rc::new(|_| Err(Box::new(std::io::Error::other("async")))),
        EventOptions::default(),
    )
    .unwrap();
    root.on(
        "event",
        Rc::new(|_| Err(Box::new(std::io::Error::other("test")))),
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
        Rc::new(|_| Err(Box::new(std::io::Error::other("test")))),
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
        Rc::new(|_| Err(Box::new(std::io::Error::other("message")))),
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
        Rc::new(|_| Err(Box::new(std::io::Error::other("message")))),
        EventOptions::default(),
    )
    .unwrap();
    assert!(root.bail("event", &[], None).is_err());
}

fn waterfall_step() -> EventCallback {
    Rc::new(|args| {
        let value = args[0].downcast_ref::<i64>().expect("value");
        let next = &args[1].downcast_ref::<AnyNext>().expect("next").0;
        let binding = next().expect("next result");
        let inner = binding.downcast_ref::<i64>().expect("i64");
        Ok(Some(Rc::new(value + inner)))
    })
}

fn waterfall_stop() -> EventCallback {
    Rc::new(|args| {
        let value = args[0].downcast_ref::<i64>().expect("value");
        Ok(Some(Rc::new(*value)))
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
            Rc::new(|| Some(Rc::new(2i64))),
        )
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
            Rc::new(|| Some(Rc::new(2i64))),
        )
        .unwrap()
        .expect("result");
    assert_eq!(result.downcast_ref::<i64>().unwrap(), &3);
}

#[tokio::test(flavor = "current_thread")]
async fn internal_update_hook() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
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
                    Rc::new(move |args| {
                        let config = args[0].downcast_ref::<Config>().expect("config").value;
                        hook_seen.borrow_mut().push(("hook", config));
                        let next = &args[3].downcast_ref::<AnyNext>().expect("next").0;
                        next();
                        Ok(None)
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
