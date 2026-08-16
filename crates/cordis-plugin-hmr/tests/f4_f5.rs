//! Story cards F4/F5: artifact hash naming, build manifest, i18n messages.

use std::fs;

use cordis_plugin_hmr::build::{BuildTarget, build_manifest, content_hash, planned_artifact};
use cordis_plugin_hmr::{HmrConfig, validate_config, validate_message};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cordis-f45-{tag}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// F4.1: content hashing is deterministic and changes with content.
#[test]
fn content_hash_changes_with_content() {
    let first = content_hash(b"plugin v1");
    let second = content_hash(b"plugin v1");
    assert_eq!(first, second, "hash must be deterministic");
    assert_ne!(first, content_hash(b"plugin v2"));
    assert_eq!(first.len(), 12);
}

/// F4.1: artifact names use the `name@hash.ext` convention.
#[test]
fn artifact_name_is_content_addressed() {
    let dir = temp_dir("names");
    let artifact = if cfg!(target_os = "macos") {
        "libdemo.dylib"
    } else {
        "libdemo.so"
    };
    fs::write(dir.join(artifact), b"binary payload").unwrap();
    let target = BuildTarget {
        name: "demo".to_string(),
        deps: vec!["cordis-core".to_string()],
        artifact: artifact.to_string(),
    };
    let name = planned_artifact(&target, &dir).unwrap();
    let extension = if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    assert_eq!(
        name,
        format!("demo@{}.{extension}", content_hash(b"binary payload"))
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// F4.2: the manifest carries `name@hash.artifact` and declared deps.
#[test]
fn manifest_lists_artifacts_and_deps() {
    let dir = temp_dir("manifest");
    let artifact = if cfg!(target_os = "macos") {
        "liba.dylib"
    } else {
        "liba.so"
    };
    fs::write(dir.join(artifact), b"a-content").unwrap();
    let manifest = build_manifest(
        &[BuildTarget {
            name: "plugin-a".to_string(),
            deps: vec!["dep".to_string()],
            artifact: artifact.to_string(),
        }],
        &dir,
    )
    .unwrap();
    assert_eq!(manifest.len(), 1);
    assert_eq!(manifest[0].name, "plugin-a");
    assert_eq!(manifest[0].deps, vec!["dep".to_string()]);
    assert!(manifest[0].artifact.starts_with("plugin-a@"));
    fs::remove_dir_all(&dir).unwrap();
}

/// F5.1: validation messages are provided in en-US and zh-CN.
#[test]
fn validation_messages_are_localized() {
    assert_eq!(
        validate_message("en-US", "debounce"),
        "hmr.config.debounce: must be a positive integer"
    );
    assert_eq!(
        validate_message("zh-CN", "debounce"),
        "hmr.config.debounce: 必须为正整数"
    );
    assert_eq!(
        validate_message("en-US", "root"),
        "hmr.config.root: must be a non-empty array"
    );
}

/// F5.2: config defaults are stable (regression against the TS defaults).
#[test]
fn config_defaults_are_stable() {
    let config = HmrConfig::default();
    assert_eq!(config.root, vec![".".to_string()]);
    assert_eq!(config.debounce, 100);
    assert_eq!(
        config.ignored,
        vec![
            "**/node_modules".to_string(),
            "**/.*".to_string(),
            "cache".to_string(),
            "data".to_string(),
        ]
    );
    assert!(validate_config(&config).is_ok());
}
