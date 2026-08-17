//! EntryGroup and the Group plugin.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use cordis_core::{Context, Effect, sync_disposer};
use cordis_loader::{EntryOptions, Loader, PartialEntryOptions};

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

fn foo_opts() -> EntryOptions {
    EntryOptions {
        id: String::new(),
        name: "foo".to_string(),
        config: None,
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: Default::default(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn group_initialize_and_disable_chain() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let foo_count = Rc::new(Cell::new(0u32));
            let dispose_count = Rc::new(Cell::new(0u32));
            let foo_count_apply = foo_count.clone();
            let dispose_count_apply = dispose_count.clone();
            loader.mock(
                "foo",
                Rc::new(move |_ctx: &Context, _config| {
                    foo_count_apply.set(foo_count_apply.get() + 1);
                    let dispose_count = dispose_count_apply.clone();
                    Effect::Disposer(sync_disposer(move || {
                        dispose_count.set(dispose_count.get() + 1);
                    }))
                }),
            );
            let tree = loader.tree_handle();

            let outer = tree.create(group_opts("", vec![foo_opts()]), None, 0);
            tree.await_tree().await;

            eprintln!(
                "[test] after outer: {:?}",
                tree.entries().iter().map(|e| e.id()).collect::<Vec<_>>()
            );
            eprintln!(
                "[test] outer subgroup len: {:?}",
                outer
                    .subgroup
                    .borrow()
                    .as_ref()
                    .map(|sg| sg.entries.borrow().len())
            );
            let inner = tree.create(group_opts("", vec![foo_opts()]), Some(&outer.id()), 0);
            tree.await_tree().await;

            assert_eq!(foo_count.get(), 2);
            assert_eq!(dispose_count.get(), 0);
            assert_eq!(tree.entries().len(), 4);

            // Disable the inner group: its subtree unloads.
            tree.update_entry(
                &inner.id(),
                PartialEntryOptions {
                    disabled: Some(true),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);
            assert_eq!(dispose_count.get(), 1);
            assert_eq!(tree.entries().len(), 4);

            // Disable the outer group: its subtree (inner's foo) unloads too,
            // but the inner's foo was already disposed.
            tree.update_entry(
                &outer.id(),
                PartialEntryOptions {
                    disabled: Some(true),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);
            eprintln!(
                "[test] after disable outer entries: {:?}",
                tree.entries().iter().map(|e| e.id()).collect::<Vec<_>>()
            );
            let outer_entry = tree.resolve(&outer.id()).unwrap();
            eprintln!(
                "[test] outer subgroup: {:?}",
                outer_entry
                    .subgroup
                    .borrow()
                    .as_ref()
                    .map(|sg| sg.entries.borrow().len())
            );
            assert_eq!(tree.entries().len(), 4);

            // Enable inner only: outer still disabled, nothing applies.
            tree.update_entry(
                &inner.id(),
                PartialEntryOptions {
                    disabled: Some(false),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            assert_eq!(foo_count.get(), 2);

            // Enable outer: both foo entries apply again.
            tree.update_entry(
                &outer.id(),
                PartialEntryOptions {
                    disabled: Some(false),
                    ..Default::default()
                },
            );
            tree.await_tree().await;
            // The outer re-read reconciles against its own config: the
            // dynamically-added inner group is dropped (mirrors TS
            // `group.update(config)` replacing `this.data`).
            assert_eq!(foo_count.get(), 3);
            assert_eq!(tree.entries().len(), 2);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn group_stop_disposes_subtree() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let dispose_count = Rc::new(Cell::new(0u32));
            let dispose_count_apply = dispose_count.clone();
            loader.mock(
                "foo",
                Rc::new(move |_ctx: &Context, _config| {
                    let dispose_count = dispose_count_apply.clone();
                    Effect::Disposer(sync_disposer(move || {
                        dispose_count.set(dispose_count.get() + 1);
                    }))
                }),
            );
            let tree = loader.tree_handle();
            let outer = tree.create(group_opts("", vec![foo_opts()]), None, 0);
            tree.await_tree().await;
            assert_eq!(dispose_count.get(), 0);

            // Disposing the group entry's fiber stops the whole subtree.
            loader.expect_fiber(&outer.id()).dispose().await;
            tree.await_tree().await;
            assert_eq!(dispose_count.get(), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn group_plugin_selection_follows_entry_name() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let applied_apply = applied.clone();
            loader.mock(
                "not-a-group",
                Rc::new(move |_ctx: &Context, _config| {
                    applied_apply.set(applied_apply.get() + 1);
                    Effect::None
                }),
            );
            let tree = loader.tree_handle();

            // `group: true` only carries container lifecycle semantics: the
            // entry still imports `options.name` (mirrors the TS loader).
            let flagged = tree.create(
                EntryOptions {
                    name: "not-a-group".to_string(),
                    config: None,
                    group: Some(true),
                    ..group_opts("", Vec::new())
                },
                None,
                0,
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert!(
                flagged.subgroup.borrow().is_none(),
                "the group builtin must not apply for a non-group name"
            );

            // Conversely, naming the group builtin applies it even without the
            // `group: true` flag.
            let named = tree.create(
                EntryOptions {
                    name: "@cordisjs/plugin-group".to_string(),
                    config: Some(serde_yaml_ng::to_value(Vec::<EntryOptions>::new()).unwrap()),
                    group: None,
                    ..group_opts("", Vec::new())
                },
                None,
                0,
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert!(
                named.subgroup.borrow().is_some(),
                "the group builtin must apply when selected by name"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_plugin_overrides_builtin_group_alias() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let applied = Rc::new(Cell::new(0u32));
            let applied_apply = applied.clone();
            loader.mock(
                "@cordisjs/plugin-group",
                Rc::new(move |_ctx: &Context, _config| {
                    applied_apply.set(applied_apply.get() + 1);
                    Effect::None
                }),
            );
            let tree = loader.tree_handle();
            let entry = tree.create(
                EntryOptions {
                    name: "@cordisjs/plugin-group".to_string(),
                    config: Some(serde_yaml_ng::to_value(Vec::<EntryOptions>::new()).unwrap()),
                    group: Some(true),
                    ..group_opts("", Vec::new())
                },
                None,
                0,
            );
            tree.await_tree().await;
            assert_eq!(applied.get(), 1);
            assert!(
                entry.subgroup.borrow().is_none(),
                "a user-registered plugin must shadow the builtin group alias"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn group_config_stays_raw_and_children_evaluate() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let sink = Rc::new(RefCell::new(None));
            let sink_apply = sink.clone();
            loader.mock(
                "foo",
                Rc::new(move |_ctx: &Context, config: &Rc<dyn std::any::Any>| {
                    if let Some(value) = config.downcast_ref::<serde_yaml_ng::Value>() {
                        *sink_apply.borrow_mut() = value.as_str().map(|s| s.to_string());
                    }
                    Effect::None
                }),
            );
            let tree = loader.tree_handle();
            let child = EntryOptions {
                name: "foo".to_string(),
                config: Some(
                    serde_yaml_ng::from_str("!expr env(\"CORDIS_LOADER_TEST_MISSING\") or \"Hi\"")
                        .unwrap(),
                ),
                ..foo_opts()
            };
            let outer = tree.create(group_opts("", vec![child]), None, 0);
            tree.await_tree().await;
            // The group's own config is passed through raw (it is a list of
            // entry options), while each child entry evaluates `!expr` when it
            // applies.
            assert_eq!(sink.borrow().as_deref(), Some("Hi"));
            assert!(
                outer.subgroup.borrow().is_some(),
                "the group builtin must still apply"
            );
        })
        .await;
}
