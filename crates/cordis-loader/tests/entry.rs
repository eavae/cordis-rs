//! Entry and EntryOptions (basic loader cases).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

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
        extra: Default::default(),
    }
}

fn counter_plugin(count: Rc<Cell<u32>>) -> cordis_core::ApplyFn {
    Rc::new(move |_ctx: &Context, _config: &Rc<dyn std::any::Any>| {
        count.set(count.get() + 1);
        Effect::None
    })
}

fn capture_plugin(sink: Rc<RefCell<Option<String>>>) -> cordis_core::ApplyFn {
    Rc::new(move |_ctx: &Context, config: &Rc<dyn std::any::Any>| {
        if let Some(value) = config.downcast_ref::<serde_yaml_ng::Value>() {
            *sink.borrow_mut() = value.as_str().map(|s| s.to_string());
        }
        Effect::None
    })
}

#[tokio::test(flavor = "current_thread")]
async fn config_expr_is_evaluated_at_apply() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let sink = Rc::new(RefCell::new(None));
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
            assert_eq!(sink.borrow().as_deref(), Some("Hello"));

            // `base_url()` comes from the loader's base url.
            loader.set_base_url("https://example.com");
            *sink.borrow_mut() = None;
            loader
                .read(vec![EntryOptions {
                    id: "2".to_string(),
                    name: "greeter".to_string(),
                    config: Some(serde_yaml_ng::from_str("!expr base_url() ~ \"/data\"").unwrap()),
                    ..opts("", "greeter", false)
                }])
                .await;
            assert_eq!(sink.borrow().as_deref(), Some("https://example.com/data"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn config_expr_error_fails_entry_apply() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            loader.mock("greeter", counter_plugin(applied.clone()));

            loader
                .read(vec![EntryOptions {
                    id: "1".to_string(),
                    name: "greeter".to_string(),
                    config: Some(serde_yaml_ng::from_str("!expr unknown_function()").unwrap()),
                    ..opts("", "greeter", false)
                }])
                .await;
            assert_eq!(applied.get(), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn loader_initiate_and_update() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let foo_count = Rc::new(Cell::new(0u32));
            let bar_count = Rc::new(Cell::new(0u32));
            let qux_count = Rc::new(Cell::new(0u32));
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

            assert_eq!(foo_count.get(), 1);
            assert_eq!(bar_count.get(), 1);
            assert_eq!(qux_count.get(), 0, "disabled entry must not apply");

            // Update: foo unchanged, bar removed, qux enabled.
            loader
                .read(vec![opts("1", "foo", false), opts("3", "qux", false)])
                .await;
            assert_eq!(foo_count.get(), 1, "unchanged entry must not re-apply");
            assert_eq!(bar_count.get(), 1);
            assert_eq!(qux_count.get(), 1);
            assert!(loader.entries().iter().all(|entry| entry.id() != "2"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_self_update_writes_back_config() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", counter_plugin(Rc::new(Cell::new(0u32))));
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
                    Some(Rc::new(config.clone()) as Rc<dyn std::any::Any>),
                    false,
                )
                .await
                .unwrap();

            let data = loader.data();
            assert_eq!(data.len(), 1);
            assert_eq!(data[0].config, Some(config));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn plugin_self_dispose_marks_disabled() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", counter_plugin(Rc::new(Cell::new(0u32))));
            loader.read(vec![opts("1", "foo", false)]).await;
            assert_eq!(loader.data()[0].disabled, None);

            loader.expect_fiber("1").dispose().await;
            tokio::task::yield_now().await;

            let data = loader.data();
            assert_eq!(data.len(), 1);
            assert_eq!(data[0].disabled, Some(true));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn entry_disabled_chain() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", counter_plugin(Rc::new(Cell::new(0u32))));
            loader.read(vec![opts("1", "foo", true)]).await;
            let entry = loader.entries().into_iter().next().unwrap();
            assert!(entry.disabled());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn entry_outer_stack() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", counter_plugin(Rc::new(Cell::new(0u32))));
            loader.read(vec![opts("1", "foo", false)]).await;
            // Outer stack lines reference the entry id.
            let entry = loader.entries().into_iter().next().unwrap();
            let stack = entry.get_outer_stack();
            assert!(stack.iter().any(|line| line.contains("#1")), "{stack:?}");
        })
        .await;
}
