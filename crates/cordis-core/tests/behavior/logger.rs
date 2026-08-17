//! Ported cases from `packages/core/tests/logger.spec.ts` plus the
//! formatting/color behaviors of `logger.ts`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cordis_core::{
    COLOR_16, COLOR_256, Context, Effect, LogValue, LoggerIntercept, LoggerLevel, LoggerService,
    LoggerType, Message, Plugin, Service, ShadowContext, SimpleExporter, UnknownLoggerLevel,
    format_message, service,
};

fn setup() -> (Context, Rc<RefCell<Vec<Message>>>) {
    let ctx = Context::new();
    let captured = Rc::new(RefCell::new(Vec::new()));
    let exporter = SimpleExporter {
        colors: 0,
        max_length: 10240,
        levels: Some(Rc::new(HashMap::from([(
            "default".to_string(),
            LoggerLevel::Debug,
        )]))),
        formatters: None,
        handler: {
            let captured = captured.clone();
            Rc::new(move |message| captured.borrow_mut().push(message.clone()))
        },
    };
    ctx.logger().exporter(Rc::new(exporter)).unwrap();
    (ctx, captured)
}

fn arg0(message: &Message) -> String {
    message.args[0].inspect()
}

/// A plugin that registers `value` as a service under `name` (the JS
/// counterpart of `root.plugin(FooService)` where the class name flows into
/// the plugin name).
fn named_plugin<S: Service>(name: &str, value: Rc<S>) -> Plugin {
    Plugin {
        is_group: false,
        name: Some(name.to_string()),
        inject: Vec::new(),
        apply: Rc::new(move |ctx: &Context, _config| {
            drop(ctx.provide::<S>(value.clone()).unwrap());
            Effect::None
        }),
    }
}

#[service]
struct FooService;

#[service]
impl FooService {
    pub fn action(&self, ctx: &ShadowContext) {
        ctx.logger().debug("from action");
    }
}

#[service]
struct BarService;

#[service]
impl BarService {
    pub fn action(&self, ctx: &ShadowContext) {
        ctx.logger().debug("from bar");
    }
}

#[service]
struct NestedFooService;

#[service]
impl NestedFooService {
    pub fn action(&self, ctx: &ShadowContext) {
        ctx.bar_service().expect("bar").action();
        ctx.logger().debug("from foo");
    }
}

#[tokio::test]
async fn keeps_bounded_buffer_in_place_and_chronological() {
    let ctx = Context::new();
    let logger = ctx.logger();
    logger.set_buffer_size(2);
    logger.info("one");
    logger.info("two");
    logger.info("three");
    assert_eq!(
        logger.buffer().iter().map(arg0).collect::<Vec<_>>(),
        vec!["two", "three"]
    );

    logger.set_buffer_size(1);
    logger.info("four");
    assert_eq!(
        logger.buffer().iter().map(arg0).collect::<Vec<_>>(),
        vec!["four"]
    );

    logger.set_buffer_size(0);
    logger.info("five");
    assert_eq!(logger.buffer().len(), 0);
}

#[tokio::test]
async fn disposes_the_exporter_that_registered_the_disposer() {
    let ctx = Context::new();
    let logger = ctx.logger();
    let first = Rc::new(RefCell::new(Vec::new()));
    let second = Rc::new(RefCell::new(Vec::new()));
    let dispose_first = logger
        .exporter(SimpleExporter::capturing(first.clone()))
        .unwrap();
    let dispose_second = logger
        .exporter(SimpleExporter::capturing(second.clone()))
        .unwrap();

    dispose_first.dispose().await.unwrap();
    logger.info("test");
    assert_eq!(first.borrow().len(), 0);
    assert_eq!(second.borrow().len(), 1);

    dispose_second.dispose().await.unwrap();
    logger.info("test");
    assert_eq!(second.borrow().len(), 1);
}

#[tokio::test]
async fn uses_fiber_name_when_called_outside_any_service() {
    let (ctx, captured) = setup();
    ctx.logger().debug("hello");
    assert_eq!(
        captured
            .borrow()
            .iter()
            .map(|message| message.name.clone())
            .collect::<Vec<_>>(),
        vec!["root"]
    );
}

#[tokio::test]
async fn honours_explicit_name_argument() {
    let (ctx, captured) = setup();
    ctx.logger().named("custom").debug("hello");
    assert_eq!(
        captured
            .borrow()
            .iter()
            .map(|message| message.name.clone())
            .collect::<Vec<_>>(),
        vec!["custom"]
    );
}

#[tokio::test]
async fn honours_intercept_name() {
    let (ctx, captured) = setup();
    let intercepted = ctx.intercept(
        "logger",
        LoggerIntercept {
            name: Some("intercepted".to_string()),
            level: None,
        },
    );
    intercepted.logger().debug("hello");
    assert_eq!(
        captured
            .borrow()
            .iter()
            .map(|message| message.name.clone())
            .collect::<Vec<_>>(),
        vec!["intercepted"]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn uses_service_name_inside_service_method() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ctx, captured) = setup();
            let fiber = ctx.plugin(&named_plugin("foo:driver", Rc::new(FooService)), None);
            fiber.wait().await.unwrap();

            // The traced handle's context carries the service's shadow, so
            // `ctx.logger()` inside the method falls back to the service's
            // own fiber name (JS: `symbols.caller` → fiber name).
            ctx.foo_service().expect("foo").action();
            let names: Vec<String> = captured
                .borrow()
                .iter()
                .map(|message| message.name.clone())
                .collect();
            assert!(names.contains(&"foo:driver".to_string()));
            assert!(!names.contains(&"root".to_string()));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn lets_outer_caller_intercept_override_service_name() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ctx, captured) = setup();
            let fiber = ctx.plugin(&named_plugin("foo:driver", Rc::new(FooService)), None);
            fiber.wait().await.unwrap();

            // The intercept chain comes from the caller's context, not the
            // service's own (JS: `this.ctx` reads the caller's intercept).
            let intercepted = ctx.intercept(
                "logger",
                LoggerIntercept {
                    name: Some("caller-override".to_string()),
                    level: None,
                },
            );
            intercepted.foo_service().expect("foo").action();
            let names: Vec<String> = captured
                .borrow()
                .iter()
                .map(|message| message.name.clone())
                .collect();
            assert!(names.contains(&"caller-override".to_string()));
            assert!(!names.contains(&"foo:driver".to_string()));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn uses_innermost_service_name_and_restores_outer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ctx, captured) = setup();
            let bar = ctx.plugin(&named_plugin("bar:driver", Rc::new(BarService)), None);
            bar.wait().await.unwrap();
            let foo = ctx.plugin(&named_plugin("foo:driver", Rc::new(NestedFooService)), None);
            foo.wait().await.unwrap();

            // No stack is involved: each method's traced context resolves
            // the name from its own shadow fiber, so bar logs first and
            // foo's log is restored when control returns (JS: per-access
            // re-derivation).
            ctx.nested_foo_service().expect("foo").action();
            let pairs: Vec<(String, String)> = captured
                .borrow()
                .iter()
                .map(|message| (message.name.clone(), arg0(message)))
                .collect();
            assert_eq!(
                pairs,
                vec![
                    ("bar:driver".to_string(), "from bar".to_string()),
                    ("foo:driver".to_string(), "from foo".to_string()),
                ]
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn uses_service_name_in_apply() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (ctx, captured) = setup();
            let plugin = Plugin {
                is_group: false,
                name: Some("foo:driver".to_string()),
                inject: Vec::new(),
                apply: Rc::new(|ctx: &Context, _config| {
                    // The apply callback is the Rust counterpart of
                    // `Service.init`: the fiber already carries the plugin
                    // name.
                    ctx.logger().debug("from init");
                    drop(ctx.provide::<FooService>(Rc::new(FooService)).unwrap());
                    Effect::None
                }),
            };
            let fiber = ctx.plugin(&plugin, None);
            fiber.wait().await.unwrap();

            let names: Vec<String> = captured
                .borrow()
                .iter()
                .map(|message| message.name.clone())
                .collect();
            assert!(names.contains(&"foo:driver".to_string()));
            assert!(!names.contains(&"root".to_string()));
        })
        .await;
}

#[tokio::test]
async fn intercept_overrides_explicit_name() {
    let (ctx, captured) = setup();
    let intercepted = ctx.intercept(
        "logger",
        LoggerIntercept {
            name: Some("intercepted".to_string()),
            level: None,
        },
    );
    intercepted.logger().named("explicit").debug("hello");
    assert_eq!(
        captured
            .borrow()
            .iter()
            .map(|message| message.name.clone())
            .collect::<Vec<_>>(),
        vec!["intercepted"]
    );
}

#[tokio::test]
async fn formats_specifiers() {
    let (ctx, captured) = setup();
    let logger = ctx.logger();
    logger.log_args(
        LoggerType::Info,
        "%s %d %i %f %o %O %c %C %%, %z",
        vec![
            LogValue::Str("str".to_string()),
            LogValue::Num(42.7),
            LogValue::Num(3.9),
            LogValue::Num(2.5),
            LogValue::Object("{ foo: 'bar' }".to_string()),
            LogValue::Object("{\"a\":1}".to_string()),
            LogValue::Empty,
            LogValue::Str("deco".to_string()),
            LogValue::Str("z".to_string()),
        ],
    );
    let message = captured.borrow().last().unwrap().clone();
    let rendered = format_message(&message, &message_exporter(), &HashMap::new());
    assert_eq!(
        rendered,
        "str 42 3 2.5 { foo: 'bar' } {\"a\":1}  deco %, %z z"
    );
}

fn message_exporter() -> SimpleExporter {
    SimpleExporter {
        colors: 0,
        max_length: 10240,
        levels: None,
        formatters: None,
        handler: Rc::new(|_| {}),
    }
}

#[tokio::test]
async fn formats_error_first_argument_as_stack() {
    let (ctx, captured) = setup();
    let logger = ctx.logger();
    logger.log_args(
        LoggerType::Info,
        "oops",
        vec![LogValue::Error("Error: boom\n    at test".to_string())],
    );
    let message = captured.borrow().last().unwrap().clone();
    let rendered = format_message(&message, &message_exporter(), &HashMap::new());
    assert!(rendered.contains("Error: boom"), "{rendered}");
}

#[tokio::test]
async fn formats_non_string_first_argument_as_object() {
    let (ctx, captured) = setup();
    let logger = ctx.logger();
    logger.log_args(LoggerType::Info, "", vec![LogValue::Num(1.0)]);
    let message = captured.borrow().last().unwrap().clone();
    let rendered = format_message(&message, &message_exporter(), &HashMap::new());
    assert!(rendered.contains("1"), "{rendered}");
}

#[tokio::test]
async fn color_code_and_tables() {
    // `Logger.code("root", level)` uses the TS hash and color tables.
    assert_eq!(LoggerService::code("root", 0), 0);
    assert_eq!(LoggerService::code("root", 2), COLOR_256[9]);
    assert_eq!(LoggerService::code("root", 1), COLOR_16[0]);
    assert_eq!(LoggerService::code("root", 2), 41);
}

#[tokio::test]
async fn color_ansi_rendering() {
    assert_eq!(LoggerService::color(0, 41, "x", ""), "x");
    assert_eq!(LoggerService::color(1, 6, "x", ""), "\u{1b}[36mx\u{1b}[0m");
    assert_eq!(
        LoggerService::color(2, 41, "x", ""),
        "\u{1b}[38;5;41mx\u{1b}[0m"
    );
    assert_eq!(
        LoggerService::color(3, 41, "x", "b"),
        "\u{1b}[38;5;41bmx\u{1b}[0m"
    );
}

#[tokio::test]
async fn level_filtering() {
    let ctx = Context::new();
    let captured = Rc::new(RefCell::new(Vec::new()));
    let exporter = SimpleExporter {
        colors: 0,
        max_length: 10240,
        levels: Some(Rc::new(HashMap::from([(
            "default".to_string(),
            LoggerLevel::Info,
        )]))),
        formatters: None,
        handler: {
            let captured = captured.clone();
            Rc::new(move |message| captured.borrow_mut().push(message.clone()))
        },
    };
    ctx.logger().exporter(Rc::new(exporter)).unwrap();

    ctx.logger().debug("hidden");
    ctx.logger().info("shown");
    ctx.logger().error("oops");
    assert_eq!(captured.borrow().len(), 2);
    assert_eq!(arg0(&captured.borrow()[0]), "shown");
    assert_eq!(arg0(&captured.borrow()[1]), "oops");
}

#[tokio::test]
async fn hyphenate_names() {
    assert_eq!(cordis_core::hyphenate("root"), "root");
    assert_eq!(cordis_core::hyphenate("FooBar"), "foo-bar");
    assert_eq!(cordis_core::hyphenate("foo:driver"), "foo:driver");
}

#[test]
fn logger_level_from_u8() {
    assert_eq!(LoggerLevel::try_from(0), Ok(LoggerLevel::Error));
    assert_eq!(LoggerLevel::try_from(3), Ok(LoggerLevel::Debug));
    assert_eq!(
        LoggerLevel::try_from(4),
        Err(UnknownLoggerLevel { value: 4 })
    );
    assert_eq!(u8::from(LoggerLevel::Warn), 1);
}

#[test]
fn logger_level_serde_round_trip() {
    assert_eq!(serde_json::to_string(&LoggerLevel::Info).unwrap(), "2");
    assert_eq!(
        serde_json::from_str::<LoggerLevel>("2").unwrap(),
        LoggerLevel::Info
    );
    let err = serde_json::from_str::<LoggerLevel>("9").unwrap_err();
    assert!(err.to_string().contains("unknown logger level: 9"));
}
