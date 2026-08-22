//! Entry and EntryOptions (basic loader cases).

use parking_lot::Mutex;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cordis_core::{Context, Effect};
use cordis_loader::{EntryOptions, Loader};

fn opts(id: &str, name: &str, disabled: bool) -> EntryOptions {
    EntryOptions {
        id: id.to_string(),
        name: name.to_string(),
        config: None,
        group: None,
        disabled: disabled.then_some(true),
        inject: None,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

fn counter_plugin(count: Arc<AtomicU32>) -> cordis_core::ApplyFn {
    Arc::new(
        move |_ctx: &Context, _config: &Arc<dyn Any + Send + Sync>| {
            count.store(count.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
            Effect::None
        },
    )
}

fn capture_plugin(sink: Arc<Mutex<Option<String>>>) -> cordis_core::ApplyFn {
    Arc::new(move |_ctx: &Context, config: &Arc<dyn Any + Send + Sync>| {
        if let Some(value) = config.downcast_ref::<serde_yaml_ng::Value>() {
            *sink.lock() = value.as_str().map(String::from);
        }
        Effect::None
    })
}

#[tokio::test(flavor = "current_thread")]
async fn config_expr_is_evaluated_at_apply() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        let sink = Arc::new(Mutex::new(None));
        loader.mock("greeter", capture_plugin(sink.clone()));

        loader
            .read(vec![EntryOptions {
                id: "1".to_string(),
                name: "greeter".to_string(),
                config: Some(
                    serde_yaml_ng::from_str(
                        "!expr env(\"CORDIS_LOADER_TEST_MISSING\") or \"Hello\"",
                    )
                    .unwrap(),
                ),
                ..opts("", "greeter", false)
            }])
            .await;
        assert_eq!(sink.lock().as_deref(), Some("Hello"));

        // `base_url()` comes from the loader's base url.
        loader.set_base_url("https://example.com");
        *sink.lock() = None;
        loader
            .read(vec![EntryOptions {
                id: "2".to_string(),
                name: "greeter".to_string(),
                config: Some(serde_yaml_ng::from_str("!expr base_url() ~ \"/data\"").unwrap()),
                ..opts("", "greeter", false)
            }])
            .await;
        assert_eq!(sink.lock().as_deref(), Some("https://example.com/data"));
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn config_expr_error_fails_entry_apply() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        let applied = Arc::new(AtomicU32::new(0));
        loader.mock("greeter", counter_plugin(applied.clone()));

        loader
            .read(vec![EntryOptions {
                id: "1".to_string(),
                name: "greeter".to_string(),
                config: Some(serde_yaml_ng::from_str("!expr unknown_function()").unwrap()),
                ..opts("", "greeter", false)
            }])
            .await;
        assert_eq!(applied.load(Ordering::SeqCst), 0);
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn loader_initiate_and_update() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        let foo_count = Arc::new(AtomicU32::new(0));
        let bar_count = Arc::new(AtomicU32::new(0));
        let qux_count = Arc::new(AtomicU32::new(0));
        loader.mock("foo", counter_plugin(foo_count.clone()));
        loader.mock("bar", counter_plugin(bar_count.clone()));
        loader.mock("qux", counter_plugin(qux_count.clone()));

        loader
            .read(vec![
                opts("1", "foo", false),
                opts("2", "bar", false),
                opts("3", "qux", true),
            ])
            .await;

        assert_eq!(foo_count.load(Ordering::SeqCst), 1);
        assert_eq!(bar_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            qux_count.load(Ordering::SeqCst),
            0,
            "disabled entry must not apply"
        );

        // Update: foo unchanged, bar removed, qux enabled.
        loader
            .read(vec![opts("1", "foo", false), opts("3", "qux", false)])
            .await;
        assert_eq!(
            foo_count.load(Ordering::SeqCst),
            1,
            "unchanged entry must not re-apply"
        );
        assert_eq!(bar_count.load(Ordering::SeqCst), 1);
        assert_eq!(qux_count.load(Ordering::SeqCst), 1);
        assert!(loader.entries().iter().all(|entry| entry.id() != "2"));
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_self_update_writes_back_config() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        loader.mock("foo", counter_plugin(Arc::new(AtomicU32::new(0))));
        loader.read(vec![opts("1", "foo", false)]).await;

        let config = {
            let mut map = serde_yaml_ng::Mapping::new();
            map.insert(
                serde_yaml_ng::Value::String("a".to_string()),
                serde_yaml_ng::Value::Number(3.into()),
            );
            serde_yaml_ng::Value::Mapping(map)
        };
        let fiber = loader.expect_fiber("1");
        fiber
            .update_with(
                Some(Arc::new(config.clone()) as Arc<dyn Any + Send + Sync>),
                false,
            )
            .await
            .unwrap();

        let data = loader.data();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].config, Some(config));
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_self_dispose_marks_disabled() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        loader.mock("foo", counter_plugin(Arc::new(AtomicU32::new(0))));
        loader.read(vec![opts("1", "foo", false)]).await;
        assert_eq!(loader.data()[0].disabled, None);

        loader.expect_fiber("1").dispose().await;
        tokio::task::yield_now().await;

        let data = loader.data();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].disabled, Some(true));
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn entry_disabled_chain() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        loader.mock("foo", counter_plugin(Arc::new(AtomicU32::new(0))));
        loader.read(vec![opts("1", "foo", true)]).await;
        let entry = loader.entries().into_iter().next().unwrap();
        assert!(entry.disabled());
    }
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn entry_outer_stack() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        loader.mock("foo", counter_plugin(Arc::new(AtomicU32::new(0))));
        loader.read(vec![opts("1", "foo", false)]).await;
        // Outer stack lines reference the entry id.
        let entry = loader.entries().into_iter().next().unwrap();
        let stack = entry.get_outer_stack();
        assert!(stack.iter().any(|line| line.contains("#1")), "{stack:?}");
    }
    .await;
}
