//! Cordis logger-console plugin (Rust port).
//!
//! Port of `@cordisjs/plugin-logger-console`: renders log messages in the
//! console format.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cordis_core::{
    LogFormatter, LoggerExporter, LoggerLevel, LoggerService, LoggerType, Message, format_message,
};
use serde::Deserialize;

/// Label alignment.
#[derive(Clone, Copy, Debug, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    /// Aligns the label to the left.
    #[default]
    Left,
    /// Aligns the label to the right.
    Right,
}

/// Label style (mirrors `LabelStyle`).
#[derive(Clone, Debug, Deserialize)]
pub struct LabelStyle {
    /// The fixed label width.
    #[serde(default)]
    pub width: usize,
    /// The margin around the label.
    #[serde(default)]
    pub margin: usize,
    /// Label alignment.
    #[serde(default)]
    pub align: Align,
}

/// The console exporter config (mirrors `ConsoleExporter.Config`).
#[derive(Clone, Debug, Deserialize)]
pub struct ConsoleConfig {
    /// Color mode: `0` = disabled, `1` = 16 colors, `2+` = 256.
    #[serde(default)]
    pub colors: u8,
    /// Maximum line length before truncation.
    #[serde(default = "default_max_length")]
    pub max_length: usize,
    /// Per-name level overrides plus `default`.
    #[serde(default)]
    pub levels: Option<HashMap<String, LoggerLevel>>,
    /// Whether to show the diff for object arguments.
    #[serde(default)]
    pub show_diff: bool,
    /// The timestamp format string.
    #[serde(default = "default_show_time")]
    pub show_time: String,
    /// Optional label styling.
    #[serde(default)]
    pub label: Option<LabelStyle>,
}

fn default_max_length() -> usize {
    10240
}

fn default_show_time() -> String {
    "yyyy-MM-dd hh:mm:ss ".to_string()
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            colors: 0,
            max_length: 10240,
            levels: None,
            show_diff: false,
            show_time: default_show_time(),
            label: None,
        }
    }
}

/// A console exporter that renders messages and writes them to a sink.
pub struct ConsoleExporter {
    /// The exporter configuration.
    pub config: ConsoleConfig,
    /// Custom formatters (`%x`).
    pub formatters: Mutex<HashMap<char, LogFormatter>>,
    timestamp: AtomicU64,
    writer: Arc<dyn Fn(&str) + Send + Sync>,
}

impl ConsoleExporter {
    /// Creates an exporter writing rendered lines to `writer`.
    pub fn new(config: ConsoleConfig, writer: Arc<dyn Fn(&str) + Send + Sync>) -> Arc<Self> {
        let exporter = Arc::new(Self {
            config,
            formatters: Mutex::new(HashMap::new()),
            timestamp: AtomicU64::new(0),
            writer,
        });
        exporter
            .timestamp
            .store(Message::now_millis(), Ordering::Relaxed);
        exporter
    }

    /// Renders a message to its console representation.
    pub fn render(&self, message: &Message) -> String {
        let prefix = format!("[{}]", type_char(message.r#type));
        let margin = self.config.label.as_ref().map_or(1, |l| l.margin);
        let space = " ".repeat(margin);
        let mut output = String::new();
        let mut indent = 3 + space.len();

        if !self.config.show_time.is_empty() {
            indent += self.config.show_time.len();
            output.push_str(&LoggerService::color(
                self.config.colors,
                8,
                &self.config.show_time,
                "",
            ));
        }

        let code = LoggerService::code(&message.name, self.config.colors);
        let label = LoggerService::color(self.config.colors, code, &message.name, ";1");
        let pad_length = self.config.label.as_ref().map_or(0, |l| {
            l.width + label.chars().count() - message.name.chars().count()
        });

        match self.config.label.as_ref().map(|l| l.align) {
            Some(Align::Right) => {
                output.push_str(&pad_start(&label, pad_length));
                output.push_str(&space);
                output.push_str(&prefix);
                output.push_str(&space);
                indent += self
                    .config
                    .label
                    .as_ref()
                    .map_or(0, |l| l.width + space.len());
            }
            _ => {
                output.push_str(&prefix);
                output.push_str(&space);
                output.push_str(&pad_end(&label, pad_length));
                output.push_str(&space);
            }
        }

        let formatters = self.formatters.lock().unwrap();
        let formatted = format_message(message, self, &formatters);
        output.push_str(&formatted.replace('\n', &format!("\n{}", " ".repeat(indent))));

        if self.config.show_diff && self.timestamp.load(Ordering::Relaxed) != 0 {
            let diff = message
                .ts
                .saturating_sub(self.timestamp.load(Ordering::Relaxed));
            output.push_str(&LoggerService::color(
                self.config.colors,
                code,
                &format!(" +{}", format_duration(diff)),
                "",
            ));
        }
        self.timestamp.store(message.ts, Ordering::Relaxed);
        output
    }

    /// Overrides the diff baseline timestamp (test helper).
    pub fn set_timestamp(&self, ts: u64) {
        self.timestamp.store(ts, Ordering::Relaxed);
    }
}

impl LoggerExporter for ConsoleExporter {
    fn colors(&self) -> u8 {
        self.config.colors
    }

    fn max_length(&self) -> usize {
        self.config.max_length
    }

    fn levels(&self) -> Option<Arc<HashMap<String, LoggerLevel>>> {
        self.config
            .levels
            .as_ref()
            .map(|levels| Arc::new(levels.clone()))
    }

    fn formatters(&self) -> Option<Arc<HashMap<char, LogFormatter>>> {
        Some(Arc::new(self.formatters.lock().unwrap().clone()))
    }

    fn export(&self, message: &Message) {
        (self.writer)(&self.render(message));
    }
}

fn type_char(r#type: LoggerType) -> char {
    match r#type {
        LoggerType::Error => 'E',
        LoggerType::Warn => 'W',
        LoggerType::Info => 'I',
        LoggerType::Debug => 'D',
    }
}

fn pad_end(value: &str, width: usize) -> String {
    let current = value.chars().count();
    if current >= width {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(width - current))
    }
}

fn pad_start(value: &str, width: usize) -> String {
    let current = value.chars().count();
    if current >= width {
        value.to_string()
    } else {
        format!("{}{value}", " ".repeat(width - current))
    }
}

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}s", ms as f64 / 1000.0)
    }
}

/// Registers the console exporter on `ctx`'s logger service.
pub fn install(
    ctx: &cordis_core::Context,
    config: ConsoleConfig,
) -> Result<Arc<ConsoleExporter>, String> {
    let writer: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|line| println!("{line}"));
    let exporter = ConsoleExporter::new(config, writer);
    let logger = ctx.get::<LoggerService>().expect("logger");
    logger
        .exporter(ctx, exporter.clone())
        .map_err(|error| error.message)?;
    Ok(exporter)
}

// Re-export helpers for tests.
pub use cordis_core::LogValue as _LogValueAlias;
