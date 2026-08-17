//! Config file parsing and atomic write-back.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde_yaml_ng::{Mapping, Value};

use crate::entry::EntryOptions;
use crate::evaluator::reject_exprs;

/// Reads a yaml/json config file into entry options.
pub fn parse_config(path: &Path) -> Result<Vec<EntryOptions>, String> {
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let content = fs::read_to_string(path)
        .map_err(|error| format!("cannot read config file {path:?}: {error}"))?;
    let configs: Vec<EntryOptions> = match extension {
        "json" => serde_json::from_str(&content)
            .map_err(|error| format!("invalid json in {path:?}: {error}")),
        "yaml" | "yml" => serde_yaml_ng::from_str(&content)
            .map_err(|error| format!("invalid yaml in {path:?}: {error}")),
        other => Err(format!("extension not supported: {other}")),
    }?;
    // `!expr` is only allowed in `config`; typed fields reject expressions at
    // deserialization, and `intercept` (a value field) is checked here.
    for options in &configs {
        if let Some(intercept) = &options.intercept {
            reject_exprs(intercept, "intercept")
                .map_err(|error| format!("invalid config in {path:?}: {error}"))?;
        }
    }
    Ok(configs)
}

/// Serializes entry options to YAML with sorted keys (`id`/`name` first,
/// `config` last, the rest alphabetically).
pub fn serialize_config(configs: &[EntryOptions]) -> Result<String, String> {
    let value = Value::Sequence(configs.iter().map(to_sorted_value).collect());
    serde_yaml_ng::to_string(&value).map_err(|error| format!("cannot serialize config: {error}"))
}

/// Builds a YAML mapping in the sorted key order.
pub fn to_sorted_value(options: &EntryOptions) -> Value {
    // `id`/`name` first, `config` last, everything else (typed fields plus
    // extra keys) sorted alphabetically — mirrors `sortKeys` in entry.ts.
    let mut mapping = Mapping::new();
    mapping.insert(key("id"), Value::String(options.id.clone()));
    mapping.insert(key("name"), Value::String(options.name.clone()));
    let mut middle = std::collections::BTreeMap::new();
    if let Some(disabled) = options.disabled {
        middle.insert("disabled".to_string(), Value::Bool(disabled));
    }
    if let Some(group) = options.group {
        middle.insert("group".to_string(), Value::Bool(group));
    }
    if let Some(inject) = &options.inject {
        let list = inject
            .iter()
            .map(|name| Value::String(name.clone()))
            .collect();
        middle.insert("inject".to_string(), Value::Sequence(list));
    }
    if let Some(intercept) = &options.intercept {
        middle.insert("intercept".to_string(), intercept.clone());
    }
    if let Some(isolate) = &options.isolate {
        let map = isolate
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    crate::IsolateValue::Flag(flag) => Value::Bool(*flag),
                    crate::IsolateValue::Label(label) => Value::String(label.clone()),
                };
                (key(name), value)
            })
            .collect();
        middle.insert("isolate".to_string(), Value::Mapping(map));
    }
    for (name, value) in &options.extra {
        middle.insert(name.clone(), value.clone());
    }
    for (name, value) in middle {
        mapping.insert(key(&name), value);
    }
    if let Some(config) = &options.config {
        mapping.insert(key("config"), config.clone());
    }
    Value::Mapping(mapping)
}

fn key(name: &str) -> Value {
    Value::String(name.to_string())
}

/// Atomically writes `content` to `path` via a temp file + rename.
///
/// Fails with `cannot overwrite readonly config` when the target is not
/// writable, leaving the original file intact.
pub fn atomic_write(path: &Path, content: &str) -> Result<(), String> {
    if path.is_dir() {
        return Err("cannot overwrite readonly config".to_string());
    }
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension().and_then(|ext| ext.to_str()).unwrap_or("")
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                "cannot overwrite readonly config".to_string()
            } else {
                format!("cannot write config file: {error}")
            }
        })?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("cannot write config file: {error}"))?;
    file.flush()
        .map_err(|error| format!("cannot write config file: {error}"))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|error| format!("cannot rename config file: {error}"))
}
