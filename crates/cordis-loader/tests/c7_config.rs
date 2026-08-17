//! Story card C7: 配置文件解析与原子写回.

use std::fs;
use std::path::PathBuf;

use cordis_loader::{EntryOptions, atomic_write, parse_config, serialize_config, to_sorted_value};

fn sample_config() -> Vec<EntryOptions> {
    vec![EntryOptions {
        id: "1".to_string(),
        name: "foo".to_string(),
        config: Some(serde_yaml_ng::to_value(serde_yaml_ng::Mapping::new()).unwrap()),
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: Default::default(),
    }]
}

#[test]
fn parses_yaml_and_json_by_extension() {
    let dir = std::env::temp_dir().join(format!("cordis-c7-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let yaml_path = dir.join("cordis.yml");
    let json_path = dir.join("cordis.json");
    fs::write(&yaml_path, "- id: '1'\n  name: foo\n").unwrap();
    fs::write(&json_path, r#"[{"id": "1", "name": "foo"}]"#).unwrap();

    let yaml = parse_config(&yaml_path).unwrap();
    let json = parse_config(&json_path).unwrap();
    assert_eq!(yaml[0].name, "foo");
    assert_eq!(json[0].name, "foo");

    let bad = dir.join("cordis.txt");
    fs::write(&bad, "x").unwrap();
    let error = parse_config(&bad).unwrap_err();
    assert_eq!(error, "extension not supported: txt");

    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sorted_yaml_key_order() {
    let options = EntryOptions {
        id: "1".to_string(),
        name: "foo".to_string(),
        config: Some(serde_yaml_ng::to_value(serde_yaml_ng::Mapping::new()).unwrap()),
        group: Some(true),
        disabled: Some(false),
        inject: None,
        isolate: None,
        intercept: None,
        extra: Default::default(),
    };
    let value = to_sorted_value(&options);
    let mapping = value.as_mapping().unwrap();
    let keys: Vec<&str> = mapping.keys().filter_map(|key| key.as_str()).collect();
    assert_eq!(keys, vec!["id", "name", "disabled", "group", "config"]);
}

#[test]
fn expr_survives_round_trip() {
    let options = EntryOptions {
        config: Some(
            serde_yaml_ng::from_str::<serde_yaml_ng::Value>(
                "greeting: !expr env(\"DEMO_GREETING\") or \"Hello\"\n",
            )
            .unwrap(),
        ),
        ..sample_config().pop().unwrap()
    };
    let dumped = serialize_config(&[options]).unwrap();
    assert!(dumped.contains("!expr"), "{dumped}");
    assert!(dumped.contains("env(\"DEMO_GREETING\")"), "{dumped}");
}

#[test]
fn atomic_write_creates_and_readonly_fails_safely() {
    let dir = std::env::temp_dir().join(format!("cordis-c7w-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("cordis.yml");

    atomic_write(&path, "id: 1\n").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "id: 1\n");

    // A directory in place of the config file is not writable as a file.
    let dir_path = dir.join("blocked.yml");
    fs::create_dir(&dir_path).unwrap();
    let error = atomic_write(&dir_path, "x").unwrap_err();
    assert_eq!(error, "cannot overwrite readonly config");
    assert!(dir_path.is_dir(), "original target must remain intact");

    fs::remove_dir_all(&dir).unwrap();
}
