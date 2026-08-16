//! Config expression evaluation via minijinja (story card C5).

use std::collections::HashMap;
use std::fmt;

use serde_yaml_ng::Value;
use serde_yaml_ng::value::{Tag, TaggedValue};

/// The evaluation environment: platform, base url and env vars.
#[derive(Clone, Debug, Default)]
pub struct EvalEnv {
    /// The current platform (e.g. `darwin`, `win32`).
    pub platform: String,
    /// The config base url.
    pub base_url: String,
    /// Environment variables visible to `env()`.
    pub env_vars: HashMap<String, String>,
}

/// An expression evaluation error.
#[derive(Debug, Clone)]
pub struct EvalError {
    /// The original expression.
    pub expr: String,
    /// The error message (with line/column when available).
    pub message: String,
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to evaluate `{}`: {}", self.expr, self.message)
    }
}

/// Evaluates a single `!expr` string to a YAML value.
pub trait ConfigEvaluator {
    /// Evaluates `expr` with `env`.
    fn evaluate(&self, expr: &str, env: &EvalEnv) -> Result<Value, EvalError>;
}

/// The default minijinja-based evaluator.
pub struct MinijinjaEvaluator;

impl ConfigEvaluator for MinijinjaEvaluator {
    fn evaluate(&self, expr: &str, env: &EvalEnv) -> Result<Value, EvalError> {
        let platform = env.platform.clone();
        let base_url = env.base_url.clone();
        let vars = env.env_vars.clone();
        let mut environment = minijinja::Environment::new();
        environment.add_function("env", move |name: String| vars.get(&name).cloned());
        environment.add_function("platform", move || platform.clone());
        environment.add_function("base_url", move || base_url.clone());

        let compiled = environment
            .compile_expression(expr)
            .map_err(|error| EvalError {
                expr: expr.to_string(),
                message: error.to_string(),
            })?;
        let result = compiled
            .eval(minijinja::context! {})
            .map_err(|error| EvalError {
                expr: expr.to_string(),
                message: error.to_string(),
            })?;
        serde_yaml_ng::to_value(result).map_err(|error| EvalError {
            expr: expr.to_string(),
            message: error.to_string(),
        })
    }
}

/// Recursively evaluates every `!expr` tagged value in `value`.
pub fn evaluate_config(
    value: &Value,
    evaluator: &dyn ConfigEvaluator,
    env: &EvalEnv,
) -> Result<Value, EvalError> {
    match value {
        Value::Tagged(tagged) => {
            if tagged.tag == Tag::new("expr") {
                let expr = tagged.value.as_str().ok_or_else(|| EvalError {
                    expr: format!("{tagged:?}"),
                    message: "!expr value must be a string".to_string(),
                })?;
                evaluator.evaluate(expr, env)
            } else {
                Ok(Value::Tagged(Box::new(TaggedValue {
                    tag: tagged.tag.clone(),
                    value: evaluate_config(&tagged.value, evaluator, env)?,
                })))
            }
        }
        Value::Mapping(mapping) => {
            let mut result = serde_yaml_ng::Mapping::new();
            for (key, item) in mapping {
                result.insert(key.clone(), evaluate_config(item, evaluator, env)?);
            }
            Ok(Value::Mapping(result))
        }
        Value::Sequence(sequence) => {
            let mut result = Vec::new();
            for item in sequence {
                result.push(evaluate_config(item, evaluator, env)?);
            }
            Ok(Value::Sequence(result))
        }
        other => Ok(other.clone()),
    }
}

/// Returns an error when `value` contains any `!expr` tag (used to enforce
/// that expressions only appear in `config`/`disabled`).
pub fn reject_exprs(value: &Value, path: &str) -> Result<(), EvalError> {
    match value {
        Value::Tagged(tagged) => {
            if tagged.tag == Tag::new("expr") {
                return Err(EvalError {
                    expr: serde_yaml_ng::to_string(&tagged.value).unwrap_or_default(),
                    message: format!("!expr is not allowed in `{path}`"),
                });
            }
            reject_exprs(&tagged.value, path)
        }
        Value::Mapping(mapping) => {
            for item in mapping.values() {
                reject_exprs(item, path)?;
            }
            Ok(())
        }
        Value::Sequence(sequence) => {
            for item in sequence {
                reject_exprs(item, path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
