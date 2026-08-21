//! Logger service: levels, formatting, colors, exporters and explicit
//! naming.

use std::any::Any;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;

use serde::{Deserialize, Deserializer};

use crate::context::Context;
use crate::fiber::EffectHandle;
use crate::service::{Config, Effect, Service, sync_disposer};

/// Log level (mirrors `LoggerLevel` in logger.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(into = "u8")]
#[repr(u8)]
pub enum LoggerLevel {
    /// Error level.
    Error = 0,
    /// Warning level.
    Warn = 1,
    /// Info level.
    Info = 2,
    /// Debug level.
    Debug = 3,
}

/// Error returned when a numeric log level does not match any [`LoggerLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownLoggerLevel {
    /// The offending numeric value.
    pub value: u8,
}

impl fmt::Display for UnknownLoggerLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown logger level: {}", self.value)
    }
}

impl Error for UnknownLoggerLevel {}

impl TryFrom<u8> for LoggerLevel {
    type Error = UnknownLoggerLevel;

    fn try_from(value: u8) -> Result<Self, UnknownLoggerLevel> {
        match value {
            0 => Ok(Self::Error),
            1 => Ok(Self::Warn),
            2 => Ok(Self::Info),
            3 => Ok(Self::Debug),
            other => Err(UnknownLoggerLevel { value: other }),
        }
    }
}

impl From<LoggerLevel> for u8 {
    fn from(level: LoggerLevel) -> Self {
        level as Self
    }
}

impl<'de> Deserialize<'de> for LoggerLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

/// Log type (mirrors `LoggerType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggerType {
    /// An error message.
    Error,
    /// A warning message.
    Warn,
    /// An info message.
    Info,
    /// A debug message.
    Debug,
}

impl LoggerType {
    /// The level of this log type.
    pub fn level(self) -> LoggerLevel {
        match self {
            Self::Error => LoggerLevel::Error,
            Self::Warn => LoggerLevel::Warn,
            Self::Info => LoggerLevel::Info,
            Self::Debug => LoggerLevel::Debug,
        }
    }
}

/// A custom formatter (`%x`).
pub type LogFormatter = Arc<dyn Fn(&LogValue) -> String + Send + Sync>;

/// Formatter table.
pub type FormatterTable = Arc<HashMap<char, LogFormatter>>;

/// A log argument value before formatting.
#[derive(Debug, Clone, PartialEq)]
pub enum LogValue {
    /// A string value (`%s`).
    Str(String),
    /// A number (`%d`/`%i`/`%f`).
    Num(f64),
    /// A pre-serialized object (`%o`/`%O`).
    Object(String),
    /// An error (`%s` → stack).
    Error(String),
    /// A null-ish value (`%c` → empty).
    Empty,
}

impl From<&str> for LogValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_string())
    }
}

impl From<String> for LogValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<i64> for LogValue {
    fn from(value: i64) -> Self {
        Self::Num(value as f64)
    }
}

impl LogValue {
    /// The string used by the `%o` formatter (JSON-ish inspection).
    pub fn inspect(&self) -> String {
        match self {
            Self::Str(value) => value.clone(),
            Self::Num(value) => format!("{value}"),
            Self::Object(value) => value.clone(),
            Self::Error(value) => value.clone(),
            Self::Empty => String::new(),
        }
    }
}

/// A single log message (mirrors `Message` in logger.ts).
#[derive(Debug, Clone)]
pub struct Message {
    /// Monotonic message id.
    pub sn: u64,
    /// Timestamp (milliseconds).
    pub ts: u64,
    /// Resolved logger name.
    pub name: String,
    /// Log type.
    pub r#type: LoggerType,
    /// Log level.
    pub level: LoggerLevel,
    /// Format string plus arguments.
    pub args: Vec<LogValue>,
}

impl Message {
    /// The current time in milliseconds (used by exporters for diffs).
    pub fn now_millis() -> u64 {
        now_millis()
    }
}

/// A log exporter (mirrors `Exporter` in logger.ts).
pub trait LoggerExporter {
    /// Color mode: `None`/`0` = disabled, `1` = 16 colors, `2+` = 256.
    fn colors(&self) -> u8 {
        0
    }

    /// Maximum line length.
    fn max_length(&self) -> usize {
        10240
    }

    /// Per-name level overrides plus `default`.
    fn levels(&self) -> Option<Arc<HashMap<String, LoggerLevel>>> {
        None
    }

    /// Custom formatters (`%x`).
    fn formatters(&self) -> Option<FormatterTable> {
        None
    }

    /// Receives every message that passes the level filter.
    fn export(&self, message: &Message);
}

/// A closure-based exporter for tests and simple use.
pub struct SimpleExporter {
    /// Color mode: `None`/`0` = disabled, `1` = 16 colors, `2+` = 256.
    pub colors: u8,
    /// Maximum line length.
    pub max_length: usize,
    /// Per-name level overrides plus `default`.
    pub levels: Option<Arc<HashMap<String, LoggerLevel>>>,
    /// Custom formatters (`%x`).
    pub formatters: Option<FormatterTable>,
    /// The sink that receives every exported message.
    pub handler: Arc<dyn Fn(&Message) + Send + Sync>,
}

impl SimpleExporter {
    /// Creates an exporter that records messages into a shared vector.
    pub fn capturing(captured: Arc<Mutex<Vec<Message>>>) -> Arc<Self> {
        Arc::new(Self {
            colors: 0,
            max_length: 10240,
            levels: None,
            formatters: None,
            handler: Arc::new(move |message| captured.lock().unwrap().push(message.clone())),
        })
    }
}

impl LoggerExporter for SimpleExporter {
    fn colors(&self) -> u8 {
        self.colors
    }

    fn max_length(&self) -> usize {
        self.max_length
    }

    fn levels(&self) -> Option<Arc<HashMap<String, LoggerLevel>>> {
        self.levels.clone()
    }

    fn formatters(&self) -> Option<FormatterTable> {
        self.formatters.clone()
    }

    fn export(&self, message: &Message) {
        (self.handler)(message);
    }
}

/// The `logger` intercept config (mirrors `LoggerService.Intercept`).
#[derive(Clone, Debug, Default)]
pub struct LoggerIntercept {
    /// The resolved logger name.
    pub name: Option<String>,
    /// The minimum level forwarded to exporters.
    pub level: Option<LoggerLevel>,
}

impl Config for LoggerIntercept {
    fn merge(&self, other: &Self) -> Self {
        Self {
            name: other.name.clone().or_else(|| self.name.clone()),
            level: other.level.or(self.level),
        }
    }
}

/// Logger service, available on every context as `ctx.logger`.
pub struct LoggerService {
    pub(crate) buffer: Arc<Mutex<Vec<Message>>>,
    pub(crate) buffer_size: Arc<AtomicUsize>,
    exporters: Arc<ArcSwap<HashMap<u64, Arc<dyn LoggerExporter + Send + Sync>>>>,
    write_lock: Arc<Mutex<()>>,
    sn_message: AtomicU64,
    sn_exporter: AtomicU64,
}

impl Default for LoggerService {
    fn default() -> Self {
        let service = Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
            buffer_size: Arc::new(AtomicUsize::new(1000)),
            exporters: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            write_lock: Arc::new(Mutex::new(())),
            sn_message: AtomicU64::new(0),
            sn_exporter: AtomicU64::new(0),
        };
        // The default exporter keeps the bounded in-memory buffer (mirrors
        // the `LoggerService` constructor).
        let buffer = Arc::clone(&service.buffer);
        let buffer_size = Arc::clone(&service.buffer_size);
        let buffer_exporter = Arc::new(BufferExporter {
            buffer,
            buffer_size,
        }) as Arc<dyn LoggerExporter + Send + Sync>;
        service
            .exporters
            .store(Arc::new(HashMap::from([(0, buffer_exporter)])));
        service
    }
}

impl Service for LoggerService {
    const NAME: &'static str = "logger";
}

impl LoggerService {
    /// Registers an exporter, managed by the fiber of `ctx`.
    pub fn exporter(
        &self,
        ctx: &Context,
        exporter: Arc<dyn LoggerExporter + Send + Sync>,
    ) -> Result<Arc<EffectHandle>, crate::fiber::CordisError> {
        let id = { self.sn_exporter.fetch_add(1, Ordering::Relaxed) + 1 };
        let exporters = self.exporters.clone();
        let write_lock = self.write_lock.clone();
        ctx.fiber().effect(
            move || {
                let _guard = write_lock.lock().unwrap();
                let mut table = (*exporters.load_full()).clone();
                table.insert(id, exporter.clone());
                exporters.store(Arc::new(table));
                drop(_guard);
                let exporters = exporters.clone();
                let write_lock = write_lock.clone();
                Effect::Disposer(sync_disposer(move || {
                    let _guard = write_lock.lock().unwrap();
                    let mut table = (*exporters.load_full()).clone();
                    table.remove(&id);
                    exporters.store(Arc::new(table));
                }))
            },
            "ctx.logger.exporter()",
        )
    }

    /// The exporter count (used by tests).
    pub fn exporter_count(&self) -> usize {
        self.exporters.load_full().len()
    }

    /// Resolves the effective name: intercept config → explicit name →
    /// fiber name (hyphenated).
    ///
    /// The intercept chain comes from `intercept` (the caller's context)
    /// while the fallback fiber name comes from `fiber` (the service's own
    /// shadow) — mirroring the TS logger, where `this.ctx` reads the
    /// caller's intercept chain and `symbols.caller` picks the logging
    /// service's fiber.
    pub(crate) fn resolve_name(
        &self,
        intercept: &Context,
        fiber: &Context,
        explicit: Option<&str>,
    ) -> String {
        let intercept = self.intercept_config(intercept);
        if let Some(name) = intercept.name {
            return name;
        }
        if let Some(name) = explicit {
            return name.to_string();
        }
        hyphenate(&fiber.fiber().name())
    }

    /// Resolves the effective level from the intercept config.
    pub(crate) fn intercept_config(&self, ctx: &Context) -> LoggerIntercept {
        let mut configs: Vec<Arc<dyn Any + Send + Sync>> = Vec::new();
        let mut layer = Some(ctx.inner.overlay.clone());
        while let Some(current) = layer {
            let state = current.load();
            if let Some(config) = state.intercept.get("logger") {
                configs.push(config.clone());
            }
            layer = state.parent.clone();
        }
        configs.reverse();
        let mut result = LoggerIntercept::default();
        for config in configs {
            let config = config
                .downcast_ref::<LoggerIntercept>()
                .expect("logger intercept config type mismatch");
            if config.name.is_some() {
                result.name = config.name.clone();
            }
            if config.level.is_some() {
                result.level = config.level;
            }
        }
        result
    }

    /// Emits a message to all exporters that pass the level filter.
    pub(crate) fn log(&self, _ctx: &Context, r#type: LoggerType, name: &str, args: Vec<LogValue>) {
        let level = r#type.level();
        let sn = { self.sn_message.fetch_add(1, Ordering::Relaxed) + 1 };
        let ts = now_millis();
        let exporters = self.exporters.load_full();
        for exporter in exporters.values() {
            let target = exporter
                .levels()
                .and_then(|levels| levels.get(name).copied())
                .or_else(|| {
                    exporter
                        .levels()
                        .and_then(|levels| levels.get("default").copied())
                })
                .unwrap_or(LoggerLevel::Info);
            if target < level {
                continue;
            }
            let message = Message {
                sn,
                ts,
                name: name.to_string(),
                r#type,
                level,
                args: args.clone(),
            };
            exporter.export(&message);
        }
    }

    fn push_buffer(&self, message: Message) {
        let size = self.buffer_size.load(Ordering::Relaxed);
        let mut buffer = self.buffer.lock().unwrap();
        buffer.push(message);
        let overflow = buffer.len().saturating_sub(size);
        if overflow > 0 {
            buffer.drain(..overflow);
        }
    }

    /// The current message buffer.
    pub fn buffer(&self) -> Vec<Message> {
        self.buffer.lock().unwrap().clone()
    }

    /// Adjusts the buffer size.
    pub fn set_buffer_size(&self, size: usize) {
        self.buffer_size.store(size, Ordering::Relaxed);
        let mut buffer = self.buffer.lock().unwrap();
        let overflow = buffer.len().saturating_sub(size);
        if overflow > 0 {
            buffer.drain(..overflow);
        }
    }
}

/// The built-in exporter that maintains the bounded message buffer.
struct BufferExporter {
    buffer: Arc<Mutex<Vec<Message>>>,
    buffer_size: Arc<AtomicUsize>,
}

impl LoggerExporter for BufferExporter {
    fn export(&self, message: &Message) {
        let size = self.buffer_size.load(Ordering::Relaxed);
        let mut buffer = self.buffer.lock().unwrap();
        buffer.push(message.clone());
        let overflow = buffer.len().saturating_sub(size);
        if overflow > 0 {
            buffer.drain(..overflow);
        }
    }
}

/// A logger bound to a context (mirrors the callable `ctx.logger(name)`).
#[derive(Clone)]
pub struct Logger {
    service: Arc<LoggerService>,
    /// The intercept chain (the caller's context; JS `this.ctx`).
    ctx: Context,
    /// The fiber used for the fallback name (the service's own shadow; JS
    /// `symbols.caller`).
    fiber_ctx: Context,
    /// The resolved logger name (see [`Logger::resolved_name`]).
    pub name: String,
    explicit: Option<String>,
}

impl Logger {
    /// Creates a logger for `ctx` with an explicit name. Both the intercept
    /// chain and the fallback fiber name come from `ctx`.
    pub fn new(ctx: &Context, explicit: Option<&str>) -> Self {
        Self::traced(ctx, ctx, explicit)
    }

    /// Creates a traced logger for a service method: the intercept chain
    /// comes from the caller's context while the fallback name comes from
    /// the service's own shadow fiber (mirrors the TS logger's traceable
    /// naming).
    pub fn traced(caller: &Context, own: &Context, explicit: Option<&str>) -> Self {
        let service = caller
            .get::<LoggerService>()
            .expect("logger service must be present");
        let name = service.resolve_name(caller, own, explicit);
        Self {
            service,
            ctx: caller.clone(),
            fiber_ctx: own.clone(),
            name,
            explicit: explicit.map(String::from),
        }
    }

    /// Returns a logger with an explicit name.
    pub fn named(&self, name: &str) -> Self {
        let mut logger = self.clone();
        logger.explicit = Some(name.to_string());
        logger.name = self
            .service
            .resolve_name(&self.ctx, &self.fiber_ctx, Some(name));
        logger
    }

    /// Registers an exporter through this logger's context.
    pub fn exporter(
        &self,
        exporter: Arc<dyn LoggerExporter + Send + Sync>,
    ) -> Result<Arc<EffectHandle>, crate::fiber::CordisError> {
        self.service.exporter(&self.ctx, exporter)
    }

    /// The current message buffer.
    pub fn buffer(&self) -> Vec<Message> {
        self.service.buffer()
    }

    /// Adjusts the buffer size.
    pub fn set_buffer_size(&self, size: usize) {
        self.service.set_buffer_size(size);
    }

    /// Re-resolves the name from intercepts (used after ctx changes).
    pub fn resolved_name(&self) -> String {
        self.service
            .resolve_name(&self.ctx, &self.fiber_ctx, self.explicit.as_deref())
    }

    /// Logs an error.
    pub fn error(&self, format: impl Into<LogValue>) {
        self.log(LoggerType::Error, vec![format.into()]);
    }

    /// Logs a warning.
    pub fn warn(&self, format: impl Into<LogValue>) {
        self.log(LoggerType::Warn, vec![format.into()]);
    }

    /// Logs an info message.
    pub fn info(&self, format: impl Into<LogValue>) {
        self.log(LoggerType::Info, vec![format.into()]);
    }

    /// Logs a debug message.
    pub fn debug(&self, format: impl Into<LogValue>) {
        self.log(LoggerType::Debug, vec![format.into()]);
    }

    /// Logs with a format string and arguments.
    pub fn log_args(&self, r#type: LoggerType, format: &str, args: Vec<LogValue>) {
        let mut all = vec![LogValue::Str(format.to_string())];
        all.extend(args);
        self.log(r#type, all);
    }

    fn log(&self, r#type: LoggerType, args: Vec<LogValue>) {
        let name = self.resolved_name();
        self.service.log(&self.ctx, r#type, &name, args);
    }
}

impl fmt::Debug for Logger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Logger").field("name", &self.name).finish()
    }
}

/// Formats a message the same way as `Logger.format` in logger.ts.
pub fn format_message(
    message: &Message,
    exporter: &dyn LoggerExporter,
    formatters: &HashMap<char, LogFormatter>,
) -> String {
    let mut args = message.args.clone();
    let format = match args.first() {
        Some(LogValue::Error(_)) => {
            let stack = match args.remove(0) {
                LogValue::Error(stack) => stack,
                _ => unreachable!(),
            };
            args.insert(0, LogValue::Str(stack));
            "%s".to_string()
        }
        Some(LogValue::Str(_)) => match args.remove(0) {
            LogValue::Str(format) => format,
            _ => unreachable!(),
        },
        _ => "%o".to_string(),
    };

    let mut output = String::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '%'
            && let Some(&next) = chars.peek()
        {
            chars.next();
            if next == '%' {
                output.push('%');
                continue;
            }
            if next.is_ascii_alphabetic() {
                let value = args.first().cloned();
                let (rendered, consumed) = match (next, value) {
                    ('s', Some(value)) => (
                        match value {
                            LogValue::Str(s) => s,
                            other => other.inspect(),
                        },
                        true,
                    ),
                    ('d', Some(LogValue::Num(n))) | ('i', Some(LogValue::Num(n))) => {
                        (format!("{}", n.trunc() as i64), true)
                    }
                    ('f', Some(LogValue::Num(n))) => (format!("{n}"), true),
                    ('o', Some(value)) | ('O', Some(value)) => (value.inspect(), true),
                    ('c', _) => (String::new(), true),
                    ('C', Some(value)) => {
                        let code = LoggerService::code(
                            &message.name,
                            if exporter.colors() >= 2 {
                                2
                            } else {
                                exporter.colors()
                            },
                        );
                        (
                            LoggerService::color(exporter.colors(), code, &value.inspect(), ""),
                            true,
                        )
                    }
                    (other, Some(value)) => {
                        if let Some(custom) = formatters.get(&other) {
                            (custom(&value), true)
                        } else {
                            (format!("%{other}"), false)
                        }
                    }
                    (other, None) => {
                        if let Some(custom) = formatters.get(&other) {
                            (custom(&LogValue::Empty), true)
                        } else {
                            (format!("%{other}"), false)
                        }
                    }
                };
                if consumed && !args.is_empty() {
                    args.remove(0);
                }
                output.push_str(&rendered);
                continue;
            }
        }
        output.push(ch);
    }

    // Remaining arguments are appended with a leading space; objects use `%o`.
    for mut arg in args {
        if matches!(arg, LogValue::Object(_)) {
            arg = LogValue::Str(arg.inspect());
        }
        output.push(' ');
        output.push_str(&arg.inspect());
    }

    let max_length = exporter.max_length();
    output
        .split('\n')
        .map(|line| {
            if line.chars().count() > max_length {
                format!("{}...", line.chars().take(max_length).collect::<String>())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The 16-color table (mirrors `c16`).
pub const COLOR_16: [u8; 6] = [6, 2, 3, 4, 5, 1];

/// The 256-color table (mirrors `c256`).
pub const COLOR_256: [u8; 75] = [
    20, 21, 26, 27, 32, 33, 38, 39, 40, 41, 42, 43, 44, 45, 56, 57, 62, 63, 68, 69, 74, 75, 76, 77,
    78, 79, 80, 81, 92, 93, 98, 99, 112, 113, 129, 134, 135, 148, 149, 160, 161, 162, 163, 164,
    165, 166, 167, 168, 169, 170, 171, 172, 173, 178, 179, 184, 185, 196, 197, 198, 199, 200, 201,
    202, 203, 204, 205, 206, 207, 208, 209, 214, 215, 220, 221,
];

impl LoggerService {
    /// Computes the color code for a name (mirrors `Logger.code`).
    pub fn code(name: &str, level: u8) -> u8 {
        let mut hash: i32 = 0;
        for ch in name.chars() {
            hash = ((hash << 3).wrapping_sub(hash))
                .wrapping_add(ch as i32)
                .wrapping_add(13);
        }
        let colors = if level == 0 {
            &[][..]
        } else if level >= 2 {
            &COLOR_256[..]
        } else {
            &COLOR_16[..]
        };
        if colors.is_empty() {
            0
        } else {
            colors[hash.unsigned_abs() as usize % colors.len()]
        }
    }

    /// Renders an ANSI-colored value (mirrors `Logger.color`).
    pub fn color(colors: u8, code: u8, value: &str, decoration: &str) -> String {
        if colors == 0 {
            return value.to_string();
        }
        let code_part = if code < 8 {
            format!("{code}")
        } else {
            format!("8;5;{code}")
        };
        let decoration_part = if colors >= 2 { decoration } else { "" };
        format!("\u{1b}[3{code_part}{decoration_part}m{value}\u{1b}[0m")
    }
}

/// Hyphenates a camelCase/PascalCase name (mirrors `cosmokit.hyphenate`).
pub fn hyphenate(name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in name.chars().enumerate() {
        if ch.is_uppercase() {
            if index > 0 {
                out.push('-');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Records an error into the buffer sink (kept for compatibility).
impl LoggerService {
    /// Records an error directly into the buffer (kept for the fiber error
    /// sink; exporters supersede it for user-facing logs).
    pub fn error(&self, message: impl fmt::Display) {
        let sn = { self.sn_message.fetch_add(1, Ordering::Relaxed) + 1 };
        self.push_buffer(Message {
            sn,
            ts: now_millis(),
            name: "root".to_string(),
            r#type: LoggerType::Error,
            level: LoggerLevel::Error,
            args: vec![LogValue::Str(message.to_string())],
        });
    }

    /// Number of recorded errors (kept for compatibility).
    pub fn error_count(&self) -> usize {
        self.buffer
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.r#type == LoggerType::Error)
            .count()
    }
}

// Keep `Error` import used by trait objects in public signatures.
#[allow(unused_imports)]
use Error as _;
