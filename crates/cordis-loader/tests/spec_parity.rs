//! TS spec 逐条对应补全：loader/group/isolate 中未 1:1 覆盖的行为用例
//! （对应 `packages/loader/tests/{group,index,isolate}.spec.ts`）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use cordis_core::{Context, Effect, FiberState, sync_disposer};
use cordis_loader::{EntryOptions, IsolateValue, Loader, LoaderIntercept, PartialEntryOptions};

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
        extra: Default::default(),
    }
}

fn group_opts(id: &str, config: Vec<EntryOptions>) -> EntryOptions {
    EntryOptions {
        id: id.to_string(),
        name: "@cordisjs/plugin-group".to_string(),
        config: Some(serde_yaml_ng::to_value(config).unwrap()),
        group: Some(true),
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: Default::default(),
    }
}

fn intercept_value(json: serde_json::Value) -> serde_yaml_ng::Value {
    serde_yaml_ng::to_value(json).unwrap()
}

fn bar_value_config(value: &str) -> serde_yaml_ng::Value {
    serde_yaml_ng::to_value(serde_json::json!({ "value": value })).unwrap()
}

fn bar_plugin() -> cordis_core::ApplyFn {
    Rc::new(|ctx: &Context, config: &Rc<dyn std::any::Any>| {
        let value = config
            .downcast_ref::<serde_yaml_ng::Value>()
            .and_then(|value| value.get("value"))
            .and_then(|value| value.as_str())
            .unwrap_or("default")
            .to_string();
        drop(
            ctx.provide_str("bar", Rc::new(BarValue { value }) as Rc<dyn std::any::Any>)
                .unwrap(),
        );
        Effect::None
    })
}

fn foo_plugin(applied: Rc<Cell<u32>>, disposed: Rc<Cell<u32>>) -> cordis_core::ApplyFn {
    Rc::new(move |_ctx: &Context, _config| {
        applied.set(applied.get() + 1);
        let disposed = disposed.clone();
        Effect::Disposer(sync_disposer(move || {
            disposed.set(disposed.get() + 1);
        }))
    })
}

fn isolate_map(entries: &[(&str, IsolateValue)]) -> HashMap<String, IsolateValue> {
    entries
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not met");
}

fn get_bar(fiber: &Rc<cordis_core::Fiber>) -> Option<String> {
    fiber
        .context()
        .get_str("bar")
        .and_then(|value| value.downcast::<BarValue>().ok())
        .map(|value| value.value.clone())
}

/// group.spec.ts "transfer": moving an entry between groups only restarts the
/// fiber when the entry becomes enabled/disabled.
#[tokio::test(flavor = "current_thread")]
async fn group_transfer_between_groups() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("foo", foo_plugin(applied.clone(), disposed.clone()));
            let tree = loader.tree_handle();

            let id = tree.create(opts("", "foo"), None, 0);
            let alpha = tree.create(group_opts("", vec![]), None, 0);
            tree.await_tree().await;
            let beta = tree.create(
                EntryOptions {
                    disabled: Some(true),
                    ..group_opts("", vec![])
                },
                Some(&alpha.id()),
                0,
            );
            tree.await_tree().await;
            let gamma = tree.create(group_opts("", vec![]), Some(&beta.id()), 0);
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 0);
            assert_eq!(tree.entries().len(), 4);

            // enabled -> enabled: no restart.
            tree.move_entry(&id.id(), Some(&alpha.id()));
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 0);

            // enabled -> disabled: unload.
            tree.move_entry(&id.id(), Some(&beta.id()));
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 1);

            // disabled -> disabled: no change.
            tree.move_entry(&id.id(), Some(&gamma.id()));
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 1);

            // disabled -> enabled: re-apply.
            tree.move_entry(&id.id(), None);
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(disposed.get(), 1);
        })
        .await;
}

/// group.spec.ts "intercept": nested group intercepts form a chain, nearest
/// entry first (mirrors the TS prototype chain `{c:3} → {b:2} → {a:1}`).
#[tokio::test(flavor = "current_thread")]
async fn group_intercept_chain() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let captured = Rc::new(RefCell::new(Vec::<serde_yaml_ng::Value>::new()));
            let captured_apply = captured.clone();
            loader.mock(
                "foo",
                Rc::new(move |ctx: &Context, _config| {
                    let chain = ctx.intercept_chain("foo");
                    let values: Vec<serde_yaml_ng::Value> = chain
                        .iter()
                        .map(|config| {
                            config
                                .downcast_ref::<serde_yaml_ng::Value>()
                                .cloned()
                                .unwrap()
                        })
                        .collect();
                    *captured_apply.borrow_mut() = values;
                    Effect::None
                }),
            );
            let tree = loader.tree_handle();

            let outer = tree.create(
                EntryOptions {
                    intercept: Some(intercept_value(serde_json::json!({ "foo": { "a": 1 } }))),
                    ..group_opts("", vec![])
                },
                None,
                0,
            );
            tree.await_tree().await;
            let inner = tree.create(
                EntryOptions {
                    intercept: Some(intercept_value(serde_json::json!({ "foo": { "b": 2 } }))),
                    ..group_opts("", vec![])
                },
                Some(&outer.id()),
                0,
            );
            tree.await_tree().await;
            tree.create(
                EntryOptions {
                    name: "foo".to_string(),
                    intercept: Some(intercept_value(serde_json::json!({ "foo": { "c": 3 } }))),
                    ..opts("", "foo")
                },
                Some(&inner.id()),
                0,
            );
            tree.await_tree().await;

            let chain = captured.borrow();
            assert_eq!(chain.len(), 3, "intercept chain must have 3 layers");
            assert_eq!(chain[0].get("c").and_then(|v| v.as_i64()), Some(3));
            assert_eq!(chain[1].get("b").and_then(|v| v.as_i64()), Some(2));
            assert_eq!(chain[2].get("a").and_then(|v| v.as_i64()), Some(1));
        })
        .await;
}

/// index.spec.ts "intercept config": fibers stay pending while the loader's
/// `await` intercept gates injects, then activate once tasks settle.
#[tokio::test(flavor = "current_thread")]
async fn loader_intercept_await_fiber_states() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            let rx_cell: Rc<RefCell<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>> =
                Rc::new(RefCell::new(Some(rx)));
            loader.mock(
                "foo",
                Rc::new(move |_ctx: &Context, _config| {
                    let mut rx = rx_cell.borrow_mut().take().expect("foo applied once");
                    Effect::Async(Box::pin(async move {
                        let _ = rx.recv().await;
                        Ok(sync_disposer(|| {}))
                    }))
                }),
            );
            loader.mock("bar", Rc::new(|_ctx, _config| Effect::None));
            loader.mock("qux", Rc::new(|_ctx, _config| Effect::None));
            root.set_intercept("loader", Rc::new(LoaderIntercept::awaiting()));

            let tree = loader.tree_handle();
            tree.create(
                EntryOptions {
                    id: "1".to_string(),
                    name: "foo".to_string(),
                    ..opts("", "foo")
                },
                None,
                0,
            );
            tree.create(
                EntryOptions {
                    id: "2".to_string(),
                    name: "bar".to_string(),
                    inject: Some(vec!["never".to_string()]),
                    ..opts("", "bar")
                },
                None,
                0,
            );
            tree.create(
                EntryOptions {
                    id: "3".to_string(),
                    name: "qux".to_string(),
                    inject: Some(vec!["loader".to_string()]),
                    intercept: Some(intercept_value(
                        serde_json::json!({ "loader": { "await": true } }),
                    )),
                    ..opts("", "qux")
                },
                None,
                0,
            );

            wait_until(|| {
                loader
                    .tree_handle()
                    .entries()
                    .iter()
                    .find(|entry| entry.options.borrow().id == "1")
                    .and_then(|entry| entry.fiber.borrow().clone())
                    .map(|fiber| fiber.state.get() == FiberState::Loading)
                    .unwrap_or(false)
            })
            .await;
            assert_eq!(
                loader.expect_fiber("2").state.get(),
                FiberState::Pending,
                "inject 'never' stays pending"
            );
            assert_eq!(
                loader.expect_fiber("3").state.get(),
                FiberState::Pending,
                "loader await gates qux while foo is loading"
            );

            let _ = tx.send(());
            wait_until(|| loader.expect_fiber("1").state.get() == FiberState::Active).await;
            wait_until(|| loader.expect_fiber("3").state.get() == FiberState::Active).await;
            assert_eq!(loader.expect_fiber("2").state.get(), FiberState::Pending);
        })
        .await;
}

/// isolate.spec.ts "basic": provider-side isolate add/remove, including the
/// irrelevant-name cases.
#[tokio::test(flavor = "current_thread")]
async fn isolate_provider_irrelevant_add_remove() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("bar", bar_plugin());
            loader.mock_with_inject(
                "foo",
                vec!["bar".to_string()],
                foo_plugin(applied.clone(), disposed.clone()),
            );
            let tree = loader.tree_handle();
            loader.read(vec![opts("1", "bar"), opts("2", "foo")]).await;
            assert_eq!(applied.get(), 1);

            // Add isolate on provider (relevant: bar).
            tree.update_entry(
                "1",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(disposed.get(), 1);

            // Add isolate (irrelevant: qux).
            tree.update_entry(
                "1",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[
                        ("bar", IsolateValue::Flag(true)),
                        ("qux", IsolateValue::Flag(true)),
                    ])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(disposed.get(), 1);

            // Remove isolate (relevant: bar gone → provider visible again).
            tree.update_entry(
                "1",
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("qux", IsolateValue::Flag(true))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(disposed.get(), 1);

            // Remove isolate (irrelevant: qux only).
            tree.update_entry(
                "1",
                PartialEntryOptions {
                    isolate: Some(HashMap::new()),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(disposed.get(), 1);
        })
        .await;
}

/// isolate.spec.ts "realm": local realms (bar: true) isolate the injector;
/// updating a group's isolate to the same value is a no-op.
#[tokio::test(flavor = "current_thread")]
async fn isolate_realm_local_and_update_no_change() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("bar", bar_plugin());
            loader.mock_with_inject(
                "foo",
                vec!["bar".to_string()],
                foo_plugin(applied.clone(), disposed.clone()),
            );
            let tree = loader.tree_handle();

            let alpha = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    config: Some(
                        serde_yaml_ng::to_value(vec![EntryOptions {
                            name: "bar".to_string(),
                            config: Some(bar_value_config("alpha")),
                            ..opts("", "bar")
                        }])
                        .unwrap(),
                    ),
                    ..group_opts("", vec![])
                },
                None,
                0,
            );
            let beta = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("beta".into()))])),
                    config: Some(
                        serde_yaml_ng::to_value(vec![EntryOptions {
                            name: "bar".to_string(),
                            config: Some(bar_value_config("beta")),
                            ..opts("", "bar")
                        }])
                        .unwrap(),
                    ),
                    ..group_opts("", vec![])
                },
                None,
                0,
            );
            tree.await_tree().await;

            let foo_alpha = tree.create(opts("", "foo"), Some(&alpha.id()), 0);
            let foo_beta = tree.create(opts("", "foo"), Some(&beta.id()), 0);
            // A local-realm injector inside alpha sees no bar at all.
            let foo_local = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    ..opts("", "foo")
                },
                Some(&alpha.id()),
                0,
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 2, "alpha and beta injectors apply");
            assert_eq!(
                get_bar(&loader.expect_fiber(&foo_alpha.id())).as_deref(),
                Some("alpha")
            );
            assert_eq!(
                get_bar(&loader.expect_fiber(&foo_beta.id())).as_deref(),
                Some("beta")
            );
            assert_eq!(
                loader.expect_fiber(&foo_local.id()).state.get(),
                FiberState::Pending
            );
            assert!(get_bar(&loader.expect_fiber(&foo_local.id())).is_none());

            // Update alpha's isolate to the same value: no change.
            tree.update_entry(
                &alpha.id(),
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(disposed.get(), 0);
        })
        .await;
}

/// isolate.spec.ts "special case: change provider": switching a group's realm
/// restarts dependents and exposes the new provider.
#[tokio::test(flavor = "current_thread")]
async fn isolate_change_provider_restarts_dependents() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("bar", bar_plugin());
            loader.mock_with_inject(
                "foo",
                vec!["bar".to_string()],
                foo_plugin(applied.clone(), disposed.clone()),
            );
            let tree = loader.tree_handle();

            tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("alpha".into()))])),
                    config: Some(bar_value_config("alpha")),
                    ..opts("", "bar")
                },
                None,
                0,
            );
            tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("beta".into()))])),
                    config: Some(bar_value_config("beta")),
                    ..opts("", "bar")
                },
                None,
                0,
            );
            // The TS special cases create the group by name only (no
            // `group: true`), so an isolate update does not restart the
            // subtree.
            let group = tree.create(
                EntryOptions {
                    name: "@cordisjs/plugin-group".to_string(),
                    config: Some(serde_yaml_ng::to_value(Vec::<EntryOptions>::new()).unwrap()),
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("alpha".into()))])),
                    ..opts("", "@cordisjs/plugin-group")
                },
                None,
                0,
            );
            tree.await_tree().await;
            let id = tree.create(opts("", "foo"), Some(&group.id()), 0);
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 0);
            assert_eq!(
                get_bar(&loader.expect_fiber(&id.id())).as_deref(),
                Some("alpha")
            );

            tree.update_entry(
                &group.id(),
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("beta".into()))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 2, "dependent must restart");
            assert_eq!(disposed.get(), 1);
            assert_eq!(
                get_bar(&loader.expect_fiber(&id.id())).as_deref(),
                Some("beta")
            );
        })
        .await;
}

/// isolate.spec.ts "special case: change injector": switching a group's realm
/// moves the provider; the losing injector unloads, the winning one applies.
#[tokio::test(flavor = "current_thread")]
async fn isolate_change_injector_switches_dependents() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("bar", bar_plugin());
            loader.mock_with_inject(
                "foo",
                vec!["bar".to_string()],
                foo_plugin(applied.clone(), disposed.clone()),
            );
            let tree = loader.tree_handle();

            let alpha = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("alpha".into()))])),
                    ..opts("", "foo")
                },
                None,
                0,
            );
            let beta = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("beta".into()))])),
                    ..opts("", "foo")
                },
                None,
                0,
            );
            let group = tree.create(
                EntryOptions {
                    name: "@cordisjs/plugin-group".to_string(),
                    config: Some(serde_yaml_ng::to_value(Vec::<EntryOptions>::new()).unwrap()),
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("alpha".into()))])),
                    ..opts("", "@cordisjs/plugin-group")
                },
                None,
                0,
            );
            tree.await_tree().await;
            tree.create(
                EntryOptions {
                    name: "bar".to_string(),
                    config: Some(bar_value_config("inner")),
                    ..opts("", "bar")
                },
                Some(&group.id()),
                0,
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 1, "only the alpha injector sees bar");
            assert!(get_bar(&loader.expect_fiber(&alpha.id())).is_some());
            assert_eq!(
                loader.expect_fiber(&beta.id()).state.get(),
                FiberState::Pending
            );

            tree.update_entry(
                &group.id(),
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Label("beta".into()))])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            wait_until(|| applied.get() == 2 && disposed.get() == 1).await;
            assert_eq!(
                loader.expect_fiber(&alpha.id()).state.get(),
                FiberState::Pending
            );
            assert_eq!(
                loader.expect_fiber(&beta.id()).state.get(),
                FiberState::Active
            );
            assert!(get_bar(&loader.expect_fiber(&beta.id())).is_some());
        })
        .await;
}

/// isolate.spec.ts "special case: nested realms": realm labels are stable
/// when ancestors change without altering the effective label.
#[tokio::test(flavor = "current_thread")]
async fn isolate_nested_realms_no_change() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("bar", bar_plugin());
            loader.mock_with_inject(
                "foo",
                vec!["bar".to_string()],
                foo_plugin(applied.clone(), disposed.clone()),
            );
            let tree = loader.tree_handle();

            // The TS nested-realms case builds groups by name only (no
            // `group: true`), so an isolate update does not restart the
            // subtree.
            let outer = tree.create(
                EntryOptions {
                    name: "@cordisjs/plugin-group".to_string(),
                    config: Some(serde_yaml_ng::to_value(Vec::<EntryOptions>::new()).unwrap()),
                    ..opts("", "@cordisjs/plugin-group")
                },
                None,
                0,
            );
            tree.await_tree().await;
            let inner = tree.create(
                EntryOptions {
                    name: "@cordisjs/plugin-group".to_string(),
                    config: Some(serde_yaml_ng::to_value(Vec::<EntryOptions>::new()).unwrap()),
                    isolate: Some(isolate_map(&[(
                        "bar",
                        IsolateValue::Label("custom".into()),
                    )])),
                    ..opts("", "@cordisjs/plugin-group")
                },
                Some(&outer.id()),
                0,
            );
            tree.await_tree().await;
            tree.create(
                EntryOptions {
                    name: "bar".to_string(),
                    config: Some(bar_value_config("custom")),
                    ..opts("", "bar")
                },
                Some(&inner.id()),
                0,
            );
            let alpha = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[(
                        "bar",
                        IsolateValue::Label("custom".into()),
                    )])),
                    ..opts("", "foo")
                },
                None,
                0,
            );
            let beta = tree.create(opts("", "foo"), Some(&inner.id()), 0);
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(
                get_bar(&loader.expect_fiber(&alpha.id())).as_deref(),
                Some("custom")
            );
            assert_eq!(
                get_bar(&loader.expect_fiber(&beta.id())).as_deref(),
                Some("custom")
            );

            // Ancestor realm changes that keep the effective label: no-op.
            tree.update_entry(
                &outer.id(),
                PartialEntryOptions {
                    isolate: Some(isolate_map(&[(
                        "bar",
                        IsolateValue::Label("custom".into()),
                    )])),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            tree.update_entry(
                &outer.id(),
                PartialEntryOptions {
                    isolate: Some(HashMap::new()),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(disposed.get(), 0);
        })
        .await;
}

/// isolate.spec.ts "transfer": moving injector/provider between realms
/// unloads/re-applies the dependent fiber.
#[tokio::test(flavor = "current_thread")]
async fn isolate_transfer_between_realms() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let disposed = Rc::new(Cell::new(0u32));
            loader.mock("bar", bar_plugin());
            loader.mock_with_inject(
                "foo",
                vec!["bar".to_string()],
                foo_plugin(applied.clone(), disposed.clone()),
            );
            let tree = loader.tree_handle();

            let group = tree.create(
                EntryOptions {
                    isolate: Some(isolate_map(&[("bar", IsolateValue::Flag(true))])),
                    ..group_opts("", vec![])
                },
                None,
                0,
            );
            let provider = tree.create(opts("", "bar"), None, 0);
            let injector = tree.create(opts("", "foo"), None, 0);
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 0);

            // Transfer injector into the isolated group: bar disappears.
            tree.move_entry(&injector.id(), Some(&group.id()));
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert_eq!(disposed.get(), 1);

            // Transfer provider into the group: bar is visible again.
            tree.move_entry(&provider.id(), Some(&group.id()));
            wait_until(|| applied.get() == 2).await;
            assert_eq!(disposed.get(), 1);

            // Transfer injector out: bar (now inside the group) is invisible
            // from the root realm.
            tree.move_entry(&injector.id(), None);
            tree.await_tree().await;
            assert_eq!(applied.get(), 2);
            assert_eq!(disposed.get(), 2);

            // Transfer provider out: bar returns to the root realm.
            tree.move_entry(&provider.id(), None);
            wait_until(|| applied.get() == 3).await;
            assert_eq!(disposed.get(), 2);
        })
        .await;
}
