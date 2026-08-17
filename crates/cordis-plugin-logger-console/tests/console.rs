//! Logger-console plugin.

use std::cell::RefCell;
use std::rc::Rc;

use cordis_core::{LogValue, LoggerLevel, LoggerType, Message};
use cordis_plugin_logger_console::{Align, ConsoleConfig, ConsoleExporter, LabelStyle};

fn message(r#type: LoggerType, name: &str, ts: u64, args: Vec<LogValue>) -> Message {
    Message {
        sn: 0,
        ts,
        name: name.to_string(),
        r#type,
        level: r#type.level(),
        args,
    }
}

fn exporter(config: ConsoleConfig, lines: Rc<RefCell<Vec<String>>>) -> Rc<ConsoleExporter> {
    let lines2 = lines;
    let exporter = ConsoleExporter::new(
        config,
        Rc::new(move |line| lines2.borrow_mut().push(line.to_string())),
    );
    // Reset the diff baseline for deterministic output.
    let base = Message::now_millis();
    exporter.set_timestamp(base);
    let _ = base;
    exporter
}

#[test]
fn formats_error() {
    let base = Message::now_millis();
    let lines = Rc::new(RefCell::new(Vec::new()));
    let exp = exporter(
        ConsoleConfig {
            colors: 0,
            show_diff: true,
            show_time: String::new(),
            ..Default::default()
        },
        lines,
    );
    exp.set_timestamp(base);
    let rendered = exp.render(&message(
        LoggerType::Error,
        "test",
        base,
        vec![LogValue::Error("message".into())],
    ));
    assert_eq!(rendered, "[E] test message +0ms");
}

#[test]
fn formats_object_with_diff() {
    let base = Message::now_millis();
    let lines = Rc::new(RefCell::new(Vec::new()));
    let exp = exporter(
        ConsoleConfig {
            colors: 0,
            show_diff: true,
            show_time: String::new(),
            ..Default::default()
        },
        lines,
    );
    exp.set_timestamp(base);
    let rendered = exp.render(&message(
        LoggerType::Info,
        "test",
        base + 2,
        vec![LogValue::Object("{ foo: 'bar' }".to_string())],
    ));
    assert_eq!(rendered, "[I] test { foo: 'bar' } +2ms");
}

#[test]
fn custom_formatter_and_escaped_percent() {
    let base = Message::now_millis();
    let lines = Rc::new(RefCell::new(Vec::new()));
    let exp = exporter(
        ConsoleConfig {
            colors: 0,
            show_time: String::new(),
            ..Default::default()
        },
        lines,
    );
    exp.formatters
        .borrow_mut()
        .insert('x', Rc::new(|_| "custom".to_string()));
    let rendered = exp.render(&message(
        LoggerType::Info,
        "test",
        base,
        vec![LogValue::Str("%x%%x".to_string())],
    ));
    assert_eq!(rendered, "[I] test custom%x");
}

#[test]
fn label_style_right_align_multiline() {
    let base = Message::now_millis();
    let lines = Rc::new(RefCell::new(Vec::new()));
    let exp = exporter(
        ConsoleConfig {
            colors: 0,
            show_diff: true,
            show_time: String::new(),
            label: Some(LabelStyle {
                width: 10,
                margin: 2,
                align: Align::Right,
            }),
            ..Default::default()
        },
        lines,
    );
    exp.set_timestamp(base);
    let rendered = exp.render(&message(
        LoggerType::Info,
        "test",
        base,
        vec![LogValue::Str("message\nmessage".to_string())],
    ));
    assert_eq!(
        rendered,
        "      test  [I]  message\n                 message +0ms"
    );
}

#[test]
fn export_filters_by_level() {
    let root = cordis_core::Context::new();
    let lines = Rc::new(RefCell::new(Vec::new()));
    let lines2 = lines.clone();
    let exporter = ConsoleExporter::new(
        ConsoleConfig {
            colors: 0,
            show_time: String::new(),
            levels: Some(std::collections::HashMap::from([(
                "default".to_string(),
                LoggerLevel::Info,
            )])),
            ..Default::default()
        },
        Rc::new(move |line| lines2.borrow_mut().push(line.to_string())),
    );
    root.logger().exporter(exporter).unwrap();
    root.logger().named("test").debug("hidden");
    root.logger().named("test").info("shown");
    assert_eq!(lines.borrow().as_slice(), &["[I] test shown"]);
}
