//! Story card C5: minijinja 配置表达式.

use std::collections::HashMap;

use cordis_loader::{ConfigEvaluator, EvalEnv, MinijinjaEvaluator, evaluate_config, reject_exprs};
use serde_yaml_ng::Value;

fn env(entries: &[(&str, &str)]) -> EvalEnv {
    EvalEnv {
        platform: "darwin".to_string(),
        base_url: "https://example.com".to_string(),
        env_vars: entries
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<HashMap<_, _>>(),
    }
}

fn eval_str(expr: &str, env: &EvalEnv) -> String {
    MinijinjaEvaluator
        .evaluate(expr, env)
        .unwrap()
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            serde_yaml_ng::to_string(&MinijinjaEvaluator.evaluate(expr, env).unwrap())
                .unwrap()
                .trim()
                .to_string()
        })
}

#[test]
fn env_var_present_and_missing() {
    let with_var = env(&[("DEMO_GREETING", "hello")]);
    assert_eq!(
        eval_str("env(\"DEMO_GREETING\") or \"Hello\"", &with_var),
        "hello"
    );
    let without_var = env(&[]);
    assert_eq!(
        eval_str("env(\"DEMO_GREETING\") or \"Hello\"", &without_var),
        "Hello"
    );
}

#[test]
fn platform_and_base_url_functions() {
    let current = env(&[]);
    assert_eq!(eval_str("platform() == \"win32\"", &current), "false");
    let win = env(&[]);
    let _ = &win;
    let mut win_env = env(&[]);
    win_env.platform = "win32".to_string();
    assert_eq!(eval_str("platform() == \"win32\"", &win_env), "true");
    assert_eq!(
        eval_str("base_url() ~ \"/data\"", &current),
        "https://example.com/data"
    );
}

#[test]
fn evaluate_config_recursively() {
    let yaml = r#"
greeting: !expr env("DEMO_GREETING") or "Hello"
port: !expr env("PORT")|int
dir: !expr base_url() ~ "/data"
"#;
    let value: Value = serde_yaml_ng::from_str(yaml).unwrap();
    let evaluated = evaluate_config(
        &value,
        &MinijinjaEvaluator,
        &env(&[("DEMO_GREETING", "hi"), ("PORT", "8080")]),
    )
    .unwrap();
    assert_eq!(evaluated["greeting"].as_str(), Some("hi"));
    assert_eq!(evaluated["port"].as_i64(), Some(8080));
    assert_eq!(evaluated["dir"].as_str(), Some("https://example.com/data"));
}

#[test]
fn reject_exprs_in_forbidden_fields() {
    let yaml = r#"
id: !expr env("ID")
"#;
    let value: Value = serde_yaml_ng::from_str(yaml).unwrap();
    let error = reject_exprs(&value, "id").unwrap_err();
    assert!(
        error.message.contains("!expr is not allowed in `id`"),
        "{error}"
    );

    let yaml = "name: plain";
    let value: Value = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(reject_exprs(&value, "name").is_ok());
}

#[test]
fn expr_round_trips_through_yaml() {
    let yaml = "greeting: !expr env(\"DEMO_GREETING\") or \"Hello\"\n";
    let value: Value = serde_yaml_ng::from_str(yaml).unwrap();
    let dumped = serde_yaml_ng::to_string(&value).unwrap();
    assert!(dumped.contains("!expr"), "{dumped}");
    assert!(dumped.contains("env(\"DEMO_GREETING\")"), "{dumped}");
}

#[test]
fn evaluation_errors_have_context() {
    let error = MinijinjaEvaluator
        .evaluate("env(\"UNKNOWN\") |nosuchfilter", &env(&[]))
        .unwrap_err();
    assert!(error.message.contains("nosuchfilter"), "{error}");

    let error = MinijinjaEvaluator
        .evaluate("unknown_function()", &env(&[]))
        .unwrap_err();
    assert!(error.message.contains("unknown_function"), "{error}");
}

#[test]
fn int_filter_and_bool_coercion() {
    assert_eq!(
        MinijinjaEvaluator
            .evaluate("env(\"PORT\")|int", &env(&[("PORT", "80")]))
            .unwrap()
            .as_i64(),
        Some(80)
    );
    assert_eq!(
        MinijinjaEvaluator
            .evaluate("\"\" or \"fallback\"", &env(&[]))
            .unwrap()
            .as_str(),
        Some("fallback")
    );
}
