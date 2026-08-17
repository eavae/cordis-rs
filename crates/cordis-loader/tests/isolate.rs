//! Loader-level isolate realms。

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use cordis_core::{Context, Effect, sync_disposer};
use cordis_loader::{EntryOptions, IsolateValue, Loader, PartialEntryOptions};

#[derive(Debug)]
struct BarValue {
    value: String,
}

fn opts(id: &str, name: &str) -> EntryOptions {
    EntryOptions {
        id: id.to_string(),
        name: name.to_string(),
        config: None,
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

fn bar_opts(id: &str, value: &'static str) -> EntryOptions {
    EntryOptions {
        config: Some(
            serde_yaml_ng::to_value(serde_yaml_ng::Mapping::from_iter([(
                serde_yaml_ng::Value::String("value".to_string()),
                serde_yaml_ng::Value::String(value.to_string()),
            )]))
            .unwrap(),
        ),
        ..opts(id, "bar")
    }
}

fn isolate_map(entries: &[(&str, IsolateValue)]) -> HashMap<String, IsolateValue> {
    entries
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

fn setup(loader: &Loader, foo_count: Rc<Cell<u32>>, dispose_count: Rc<Cell<u32>>) {
    loader.mock(
        "bar",
        Rc::new(|ctx: &Context, config: &Rc<dyn Any>| {
            let value = config
                .downcast_ref::<serde_yaml_ng::Value>()
                .and_then(|value| value.get("value"))
                .and_then(|value| value.as_str())
                .unwrap_or("default")
                .to_string();
            drop(ctx.provide_str("bar", Rc::new(BarValue { value })).unwrap());
            Effect::None
        }),
    );
    loader.mock_with_inject(
        "foo",
        vec!["bar".to_string()],
        Rc::new(move |_ctx: &Context, _config| {
            foo_count.set(foo_count.get() + 1);
            let dispose_count = dispose_count.clone();
            Effect::Disposer(sync_disposer(move || {
                dispose_count.set(dispose_count.get() + 1);
            }))
        }),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn injector_isolate_relevant_and_irrelevant() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let foo_count = Rc::new(Cell::new(0u32));
            let dispose_count = Rc::new(Cell::new(0u32));
            setup(&loader, foo_count.clone(), dispose_count.clone());
            let tree = loader.tree_handle();
            loader
                .read(vec![bar_opts("1", "root"), opts("2", "foo")])
                .await;
            assert_eq!(foo_count.get(), 1);
            assert_eq!(dispose_count.get(), 0);

            // Add isolate on the injector (relevant: bar).
            tree.update_entry(
                "2",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;

            assert_eq!(foo_count.get(), 1);
            assert_eq!(dispose_count.get(), 1);

            // Add isolate (irrelevant: qux).
            tree.update_entry(
                "2",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[
                        ("bar", IsolateValue::Flag(true)),
                        ("qux", IsolateValue::Flag(true)),
                    ])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 1);
            assert_eq!(dispose_count.get(), 1);

            // Remove isolate (relevant: bar gone → re-applies).
            tree.update_entry(
                "2",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("qux", IsolateValue::Flag(true))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);
            assert_eq!(dispose_count.get(), 1);

            // Remove isolate (irrelevant: qux only).
            tree.update_entry(
                "2",
                PartialEntryOptions {
                    isolate: Some(HashMap::new()),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);
            assert_eq!(dispose_count.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn provider_isolate_relevant() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let foo_count = Rc::new(Cell::new(0u32));
            let dispose_count = Rc::new(Cell::new(0u32));
            setup(&loader, foo_count.clone(), dispose_count.clone());
            let tree = loader.tree_handle();
            loader
                .read(vec![bar_opts("1", "root"), opts("2", "foo")])
                .await;
            assert_eq!(foo_count.get(), 1);

            // Isolating the provider moves bar away from the root label.
            tree.update_entry(
                "1",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 1);
            assert_eq!(dispose_count.get(), 1);

            // Removing the provider isolate restores access.
            tree.update_entry(
                "1",
                PartialEntryOptions {
                    isolate: Some(HashMap::new()),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);
            assert_eq!(dispose_count.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn realm_global_labels_share() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let foo_count = Rc::new(Cell::new(0u32));
            let dispose_count = Rc::new(Cell::new(0u32));
            setup(&loader, foo_count.clone(), dispose_count.clone());
            let tree = loader.tree_handle();

            // Two groups with different labels isolate bar separately.
            let alpha = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("alpha".into()))])),
                    config: Some(serde_yaml_ng::to_value(vec![bar_opts("", "alpha")]).unwrap()),
                    group: Some(true),
                    ..opts("", "@cordisjs/plugin-group")
                },
                None,
                0,
            );
            tree.await_tree().await;
            let beta = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("beta".into()))])),
                    config: Some(serde_yaml_ng::to_value(vec![bar_opts("", "beta")]).unwrap()),
                    group: Some(true),
                    ..opts("", "@cordisjs/plugin-group")
                },
                None,
                0,
            );
            tree.await_tree().await;

            // A foo under alpha sees the alpha bar; a foo under beta sees beta.
            let foo_alpha = tree.create(opts("", "foo"), Some(&alpha.id()), 0);
            let foo_beta = tree.create(opts("", "foo"), Some(&beta.id()), 0);
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);

            let value = |fiber: &Rc<cordis_core::Fiber>| {
                fiber
                    .context()
                    .get_str("bar")
                    .and_then(|value| value.downcast::<BarValue>().ok())
                    .map(|value| value.value.clone())
            };
            let alpha_fiber = loader.expect_fiber(&foo_alpha.id());
            let beta_fiber = loader.expect_fiber(&foo_beta.id());
            assert_eq!(value(&alpha_fiber).as_deref(), Some("alpha"));
            assert_eq!(value(&beta_fiber).as_deref(), Some("beta"));
        })
        .await;
}
