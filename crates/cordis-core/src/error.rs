//! Error types shared by the core runtime.

use std::error::Error;
use std::fmt;

/// A single validation issue (mirrors `StandardSchemaV1.Issue`).
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationIssue {
    /// The human-readable message.
    pub message: String,
    /// The property path, if any.
    pub path: Option<String>,
}

/// Config validation failure (mirrors `ValidationError` in fiber.ts).
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    /// All validation issues.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationError {
    /// Creates an error from a single message.
    pub fn new(message: impl Into<String>) -> Self {
        ValidationError {
            issues: vec![ValidationIssue {
                message: message.into(),
                path: None,
            }],
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid config:")?;
        for issue in &self.issues {
            match &issue.path {
                Some(path) => writeln!(f, "  - {} (at {path})", issue.message)?,
                None => writeln!(f, "  - {}", issue.message)?,
            }
        }
        Ok(())
    }
}

impl Error for ValidationError {}

/// A config validator (`fn(&Rc<dyn Any>) -> Result<(), ValidationError>`).
pub type ConfigValidator =
    std::rc::Rc<dyn Fn(&std::rc::Rc<dyn std::any::Any>) -> Result<(), ValidationError>>;
