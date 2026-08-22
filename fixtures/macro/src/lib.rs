//! A `.so` fixture plugin written with the declarative `cordis_plugin!` macro.

#![allow(missing_docs)]

use cordis_sdk::{ContextBridge, cordis_plugin};
use serde::Deserialize;
use std::sync::atomic::{AtomicU32, Ordering};

/// The plugin's config: a greeting stored as a service.
#[derive(Deserialize)]
struct Config {
    greeting: String,
}

static APPLY_COUNT: AtomicU32 = AtomicU32::new(0);
static VALIDATE_COUNT: AtomicU32 = AtomicU32::new(0);

fn validate(config: &Config) -> Result<(), String> {
    VALIDATE_COUNT.fetch_add(1, Ordering::SeqCst);
    if config.greeting.is_empty() {
        Err("greeting must not be empty".to_string())
    } else {
        Ok(())
    }
}

fn apply(bridge: &ContextBridge, config: &Config) -> Result<(), String> {
    APPLY_COUNT.fetch_add(1, Ordering::SeqCst);
    bridge
        .provide(
            "greeting",
            &serde_json::to_string(&config.greeting).expect("greeting serializes"),
        )
        .map_err(|error| error.to_string())
}

cordis_plugin! {
    meta: c"{\"name\":\"cordis-macro\",\"version\":\"0.1.0\",\"inject\":[],\"provide\":[\"greeting\"]}",
    config: Config,
    apply: apply,
    validate: validate,
}

/// Test helper: how many times apply ran.
#[unsafe(no_mangle)]
pub extern "C" fn macro_apply_count() -> u32 {
    APPLY_COUNT.load(Ordering::SeqCst)
}

/// Test helper: how many times validate ran.
#[unsafe(no_mangle)]
pub extern "C" fn macro_validate_count() -> u32 {
    VALIDATE_COUNT.load(Ordering::SeqCst)
}
