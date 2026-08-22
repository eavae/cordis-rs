//! Entry-level `inject` merging and `noSave` write-back skipping.

use parking_lot::Mutex;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use cordis_core::{Context, Effect, Fiber, FiberState, Plugin};
use cordis_loader::{EntryOptions, Loader};

fn opts(id: &str, name: &str, inject: Option<Vec<String>>) -> EntryOptions {
    EntryOptions {
        id: id.to_string(),
        name: name.to_string(),
        config: None,
        group: None,
        disabled: None,
        inject,
        isolate: None,
        intercept: None,
        extra: HashMap::default(),
    }
}

fn yaml_value(pairs: &[(&str, i64)]) -> serde_yaml_ng::Value {
    let mut map = serde_yaml_ng::Mapping::new();
    for (key, value) in pairs {
        map.insert(
            serde_yaml_ng::Value::String((*key).to_string()),
            serde_yaml_ng::Value::Number((*value).into()),
        );
    }
    serde_yaml_ng::Value::Mapping(map)
}

/// Waits until `check` returns true (bounded; the runtime is single-threaded
/// so yielding lets spawned reload tasks run).
async fn wait_until(mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not met within 200 yields");
}

/// Entry-level `inject` merges with the plugin fiber: the entry stays
/// PENDING while the dependency is missing and activates once it arrives.
#[tokio::test(flavor = "current_thread")]
async fn entry_inject_keeps_fiber_pending_until_provider() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        let applied = Arc::new(Mutex::new(0u32));
        loader.mock(
            "needs",
            Arc::new({
                let applied = applied.clone();
                move |_ctx: &Context, _config| {
                    *applied.lock() += 1;
                    Effect::None
                }
            }),
        );
        loader.mock(
            "provider",
            Arc::new(|ctx: &Context, _config| {
                drop(
                    ctx.provide_str("slot", Arc::new(()) as Arc<dyn Any + Send + Sync>)
                        .unwrap(),
                );
                Effect::None
            }),
        );

        loader
            .read(vec![opts("1", "needs", Some(vec!["slot".to_string()]))])
            .await;
        let fiber = loader.expect_fiber("1");
        assert_eq!(applied.lock().clone(), 0, "missing dependency blocks apply");
        assert!(fiber.inject.lock().contains_key("slot"));
        assert_eq!(fiber.state(), FiberState::Pending);

        // Provide `slot` by adding the provider entry; the pending entry
        // must activate through the registry notification.
        loader
            .read(vec![
                opts("1", "needs", Some(vec!["slot".to_string()])),
                opts("2", "provider", None),
            ])
            .await;
        loader.tree.await_tree().await;
        wait_until(|| fiber.state() == FiberState::Active).await;
        assert_eq!(applied.lock().clone(), 1);
        assert_eq!(fiber.state(), FiberState::Active);
    }
    .await;
}

/// `fiber.update_with(config, true)` skips the write-back hooks and does not
/// touch `entry.options.config`.
#[tokio::test(flavor = "current_thread")]
async fn no_save_skips_write_back() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        loader.mock("foo", Arc::new(|_ctx: &Context, _config| Effect::None));
        let mut options = opts("1", "foo", None);
        options.config = Some(yaml_value(&[("a", 1)]));
        loader.read(vec![options]).await;

        let fiber = loader.expect_fiber("1");
        fiber
            .update_with(
                Some(Arc::new(yaml_value(&[("a", 3)])) as Arc<dyn Any + Send + Sync>),
                true,
            )
            .await
            .unwrap();
        let data = loader.data();
        assert_eq!(
            data[0].config,
            Some(yaml_value(&[("a", 1)])),
            "noSave must not write back"
        );
    }
    .await;
}

/// A child fiber under the same entry does not write back to the entry's
/// config (mirrors `this.parent.fiber?.entry === this.entry`).
#[tokio::test(flavor = "current_thread")]
async fn child_fiber_does_not_write_back() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        let child = Arc::new(Mutex::new(None::<Arc<Fiber>>));
        let child_slot = child.clone();
        let child_plugin = Plugin {
            is_group: false,
            name: None,
            inject: Vec::new(),
            apply: Arc::new(|_ctx: &Context, _config| Effect::None),
        };
        loader.mock(
            "parent",
            Arc::new(move |ctx: &Context, _config| {
                let fiber = ctx.plugin(&child_plugin, None);
                *child_slot.lock() = Some(fiber);
                Effect::None
            }),
        );
        let mut options = opts("1", "parent", None);
        options.config = Some(yaml_value(&[("a", 1)]));
        loader.read(vec![options]).await;

        let child = child.lock().clone().expect("child fiber must be created");
        child
            .update_with(
                Some(Arc::new(yaml_value(&[("x", 9)])) as Arc<dyn Any + Send + Sync>),
                false,
            )
            .await
            .unwrap();
        let data = loader.data();
        assert_eq!(
            data[0].config,
            Some(yaml_value(&[("a", 1)])),
            "child update must not write back"
        );
    }
    .await;
}

/// The normal (`noSave = false`) root update path still writes back through
/// the tree.
#[tokio::test(flavor = "current_thread")]
async fn normal_update_still_writes_back() {
    async {
        let root = Context::new();
        let loader = Loader::new(&root);
        loader.mock("foo", Arc::new(|_ctx: &Context, _config| Effect::None));
        loader.read(vec![opts("1", "foo", None)]).await;

        let fiber = loader.expect_fiber("1");
        fiber
            .update_with(
                Some(Arc::new(yaml_value(&[("a", 3)])) as Arc<dyn Any + Send + Sync>),
                false,
            )
            .await
            .unwrap();
        let data = loader.data();
        assert_eq!(data[0].config, Some(yaml_value(&[("a", 3)])));
    }
    .await;
}
