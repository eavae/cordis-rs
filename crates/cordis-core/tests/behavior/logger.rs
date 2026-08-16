//! Ported cases from `packages/core/tests/logger.spec.ts` (story card B7)
//! plus the formatting/color behaviors of `logger.ts`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cordis_core::{
    C16, C256, Context, LogValue, LoggerIntercept, LoggerLevel, LoggerService, LoggerType, Message,
    SimpleExporter, format_message,
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
    assert_eq!(LoggerService::code("root", 2), C256[9]);
    assert_eq!(LoggerService::code("root", 1), C16[0]);
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
