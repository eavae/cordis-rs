//! Story card C6: Loader 服务本体.

use std::rc::Rc;

use cordis_core::{Context, Effect, LoggerLevel, SimpleExporter};
use cordis_loader::{EntryOptions, Loader};

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

#[tokio::test(flavor = "current_thread")]
async fn locate_returns_entry_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            loader.mock("foo", Rc::new(|_ctx, _config| Effect::None));
            loader.read(vec![opts("1", "foo")]).await;

            let fiber = loader.expect_fiber("1");
            assert_eq!(loader.locate(&fiber).as_deref(), Some("1"));
            assert_eq!(loader.locate(root.fiber()), None);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn check_reflects_await_config_and_tasks() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            assert!(loader.check(), "no tasks → available");

            // Simulate pending tasks with the await intercept enabled.
            loader.tree.tasks.set(2);
            assert!(loader.check(), "await disabled → still available");

            root.set_intercept(
                "loader",
                Rc::new(cordis_loader::LoaderIntercept::awaiting()),
            );
            assert!(!loader.check(), "await enabled + tasks → unavailable");

            loader.tree.tasks.set(0);
            assert!(loader.check());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn env_data_parses_cordis_shared() {
    let root = Context::new();
    let loader = Loader::with_shared(&root, Some(r#"{"startTime": 123}"#.to_string()));
    assert_eq!(loader.env_data["startTime"], 123);
}

#[tokio::test(flavor = "current_thread")]
async fn show_log_emits_apply_and_reload() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let captured = Rc::new(std::cell::RefCell::new(Vec::new()));
            drop(
                root.logger()
                    .exporter(Rc::new(SimpleExporter {
                        colors: 0,
                        max_length: 10240,
                        levels: Some(Rc::new(std::collections::HashMap::from([(
                            "default".to_string(),
                            LoggerLevel::Debug,
                        )]))),
                        formatters: None,
                        handler: {
                            let captured = captured.clone();
                            Rc::new(move |message| {
                                captured.borrow_mut().push(message.args[0].inspect())
                            })
                        },
                    }))
                    .unwrap(),
            );
            let loader = Loader::new(&root);
            loader.mock("foo", Rc::new(|_ctx, _config| Effect::None));
            loader.read(vec![opts("1", "foo")]).await;

            assert!(
                captured
                    .borrow()
                    .iter()
                    .any(|line| line.contains("apply plugin foo")),
                "{:?}",
                captured.borrow()
            );

            let config = serde_yaml_ng::to_value(serde_yaml_ng::Mapping::new()).unwrap();
            loader
                .expect_fiber("1")
                .update_with(Some(Rc::new(config) as Rc<dyn std::any::Any>), false)
                .await
                .unwrap();
            tokio::task::yield_now().await;
            assert!(
                captured
                    .borrow()
                    .iter()
                    .any(|line| line.contains("reload plugin foo")),
                "{:?}",
                captured.borrow()
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn loader_service_is_accessible() {
    let root = Context::new();
    let loader = Loader::new(&root);
    let from_ctx = root.get::<Loader>().expect("ctx.loader");
    assert!(Rc::ptr_eq(&loader, &from_ctx));
}
