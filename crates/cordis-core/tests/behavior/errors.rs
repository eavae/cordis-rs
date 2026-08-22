//! Ported behaviors from `fiber.ts`: ValidationError formatting, config
//! validation on registration/update, and entry location in apply errors.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{
    ConfigValidator, Context, Effect, FiberState, LoggerService, Plugin, ValidationError,
    ValidationIssue,
};

#[derive(Debug)]
struct Config {
    value: i32,
}

fn value_must_be_positive() -> ConfigValidator {
    Arc::new(
        |config: &Arc<dyn Any + Send + Sync>| -> Result<(), ValidationError> {
            let config = config.downcast_ref::<Config>().expect("config");
            if config.value > 0 {
                Ok(())
            } else {
                Err(ValidationError {
                    issues: vec![ValidationIssue {
                        message: "value must be positive".to_string(),
                        path: Some("value".to_string()),
                    }],
                })
            }
        },
    )
}

#[test]
fn validation_error_format_matches_ts() {
    let error = ValidationError {
        issues: vec![
            ValidationIssue {
                message: "value must be positive".to_string(),
                path: Some("value".to_string()),
            },
            ValidationIssue {
                message: "required".to_string(),
                path: None,
            },
        ],
    };
    assert_eq!(
        error.to_string(),
        "invalid config:\n  - value must be positive (at value)\n  - required\n"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn config_validation_rejects_registration() {
    async {
        let root = Context::new();
        let applied = Arc::new(AtomicU32::new(0));
        let fiber = root.plugin_with_validator(
            &Plugin {
                is_group: false,
                name: Some("demo".to_string()),
                inject: Vec::new(),
                apply: {
                    let applied = applied.clone();
                    Arc::new(
                        move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                            applied.store(applied.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            Effect::None
                        },
                    )
                },
            },
            Some(Arc::new(Config { value: -1 })),
            Some(value_must_be_positive()),
        );
        tokio::task::yield_now().await;
        assert!(fiber.wait().await.is_err());
        assert_eq!(fiber.state(), FiberState::Failed);
        assert_eq!(
            applied.load(Ordering::SeqCst),
            0,
            "apply must not run for invalid config"
        );
        assert!(
            root.get::<LoggerService>().unwrap().error_count() >= 1,
            "validation error must be logged"
        );
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn config_validation_rejects_update() {
    async {
        let root = Context::new();
        let applied = Arc::new(AtomicU32::new(0));
        let fiber = root.plugin_with_validator(
            &Plugin {
                is_group: false,
                name: None,
                inject: Vec::new(),
                apply: {
                    let applied = applied.clone();
                    Arc::new(
                        move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
                            applied.store(applied.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                            Effect::None
                        },
                    )
                },
            },
            Some(Arc::new(Config { value: 1 })),
            Some(value_must_be_positive()),
        );
        fiber.wait().await.unwrap();
        assert_eq!(applied.load(Ordering::SeqCst), 1);

        let error = fiber
            .update(Some(Arc::new(Config { value: -5 })))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid config"), "{error}");
        assert_eq!(
            applied.load(Ordering::SeqCst),
            1,
            "invalid update must not re-apply"
        );
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn apply_error_includes_entry_location() {
    async {
        let root = Context::new();
        let fiber = root.plugin(
            &Plugin {
                is_group: false,
                name: Some("demo".to_string()),
                inject: Vec::new(),
                apply: Arc::new(|_ctx: &Context, _config| {
                    Effect::Error(Box::new(std::io::Error::other("boom")))
                }),
            },
            None,
        );
        tokio::task::yield_now().await;
        assert!(fiber.wait().await.is_err());

        // The logged error carries the entry location (`at <name>`).
        let logger = root.get::<LoggerService>().unwrap();
        let messages = logger
            .buffer()
            .into_iter()
            .filter(|message| message.args[0].inspect().contains("boom"))
            .map(|message| message.args[0].inspect())
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|message| message.contains("at <demo>")),
            "{messages:?}"
        );
    }
    .await;
}
