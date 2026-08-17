//! Context 骨架：双轨 Service 访问与链式隔离。

use std::rc::Rc;

use cordis_core::{
    Config, Context, EventsService, FiberState, LoggerService, ReflectService, RegistryService,
    Service,
};

#[derive(Debug)]
struct Foo {
    bar: i32,
}

impl Service for Foo {
    const NAME: &'static str = "foo";
}

#[derive(Clone, Debug, PartialEq, Default)]
struct FooConfig {
    a: Option<i32>,
    b: Option<i32>,
    c: Option<i32>,
}

impl Config for FooConfig {
    fn merge(&self, other: &Self) -> Self {
        FooConfig {
            a: other.a.or(self.a),
            b: other.b.or(self.b),
            c: other.c.or(self.c),
        }
    }
}

#[derive(Debug)]
struct MetaValue(String);

#[tokio::test]
async fn root_construction_provides_four_services() {
    let root = Context::new();
    assert_eq!(root.fiber().state.get(), FiberState::Active);
    assert_eq!(root.fiber().name(), "root");

    assert!(root.get::<EventsService>().is_some());
    assert!(root.get::<LoggerService>().is_some());
    assert!(root.get::<ReflectService>().is_some());
    assert!(root.get::<RegistryService>().is_some());
    assert!(root.get_str("events").is_some());
}

#[tokio::test]
async fn get_and_provide_roundtrip() {
    let root = Context::new();
    assert!(root.get::<Foo>().is_none());
    // Missing names resolve to None without panicking.
    assert!(root.get_str("missing").is_none());

    let handle = root.provide::<Foo>(Rc::new(Foo { bar: 100 })).unwrap();
    let foo = root.get::<Foo>().expect("foo must be visible");
    assert_eq!(foo.bar, 100);

    handle.dispose().await.unwrap();
    assert!(root.get::<Foo>().is_none());
}

#[tokio::test]
async fn duplicate_provide_reports_error() {
    let root = Context::new();
    drop(root.provide::<Foo>(Rc::new(Foo { bar: 1 })).unwrap());
    let err = match root.provide::<Foo>(Rc::new(Foo { bar: 2 })) {
        Ok(_) => panic!("duplicate provide must fail"),
        Err(err) => err,
    };
    assert!(err.contains("service \"foo\" has been registered"), "{err}");
}

#[tokio::test]
async fn isolate_hides_isolated_services_from_parent() {
    let root = Context::new();
    let ctx1 = root.isolate("foo", Rc::from("label-1"));
    let ctx2 = root.isolate("foo", Rc::from("label-2"));

    let dispose0 = root.provide::<Foo>(Rc::new(Foo { bar: 100 })).unwrap();
    assert_eq!(root.get::<Foo>().unwrap().bar, 100);
    // Children isolated with fresh labels cannot see the root provider.
    assert!(ctx1.get::<Foo>().is_none());
    assert!(ctx2.get::<Foo>().is_none());

    let dispose1 = ctx1.provide::<Foo>(Rc::new(Foo { bar: 200 })).unwrap();
    assert_eq!(root.get::<Foo>().unwrap().bar, 100);
    assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
    assert!(ctx2.get::<Foo>().is_none());

    dispose0.dispose().await.unwrap();
    assert!(root.get::<Foo>().is_none());
    assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
    assert!(ctx2.get::<Foo>().is_none());

    let dispose2 = ctx2.provide::<Foo>(Rc::new(Foo { bar: 300 })).unwrap();
    assert!(root.get::<Foo>().is_none());
    assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
    assert_eq!(ctx2.get::<Foo>().unwrap().bar, 300);

    dispose1.dispose().await.unwrap();
    dispose2.dispose().await.unwrap();
}

#[tokio::test]
async fn shared_label_shares_service_instance() {
    let root = Context::new();
    let label = Rc::<str>::from("test");
    let ctx1 = root.isolate("foo", label.clone());
    let ctx2 = root.isolate("foo", label);

    let dispose0 = root.provide::<Foo>(Rc::new(Foo { bar: 100 })).unwrap();
    assert_eq!(root.get::<Foo>().unwrap().bar, 100);
    assert!(ctx1.get::<Foo>().is_none());
    assert!(ctx2.get::<Foo>().is_none());

    let dispose12 = ctx1.provide::<Foo>(Rc::new(Foo { bar: 200 })).unwrap();
    assert_eq!(root.get::<Foo>().unwrap().bar, 100);
    assert_eq!(ctx1.get::<Foo>().unwrap().bar, 200);
    assert_eq!(ctx2.get::<Foo>().unwrap().bar, 200);

    dispose12.dispose().await.unwrap();
    assert_eq!(root.get::<Foo>().unwrap().bar, 100);
    assert!(ctx1.get::<Foo>().is_none());
    assert!(ctx2.get::<Foo>().is_none());

    dispose0.dispose().await.unwrap();
}

#[tokio::test]
async fn unisolated_child_sees_parent_services() {
    let root = Context::new();
    drop(root.provide::<Foo>(Rc::new(Foo { bar: 100 })).unwrap());

    let child = root.extend(&[]);
    assert_eq!(child.get::<Foo>().unwrap().bar, 100);
}

#[tokio::test]
async fn intercept_merges_along_chain() {
    let root = Context::new();
    let ctx1 = root.intercept(
        "foo",
        FooConfig {
            a: Some(1),
            ..Default::default()
        },
    );
    let ctx2 = ctx1.intercept(
        "foo",
        FooConfig {
            b: Some(2),
            ..Default::default()
        },
    );

    // base + parent layer + nearest layer + head
    let merged = ctx2.resolve_config::<FooConfig>(
        "foo",
        Some(&FooConfig::default()),
        Some(&FooConfig {
            c: Some(3),
            ..Default::default()
        }),
    );
    assert_eq!(
        merged,
        FooConfig {
            a: Some(1),
            b: Some(2),
            c: Some(3),
        }
    );

    // Nearest layer overrides the parent layer.
    let ctx3 = ctx1.intercept(
        "foo",
        FooConfig {
            a: Some(9),
            ..Default::default()
        },
    );
    let merged = ctx3.resolve_config::<FooConfig>("foo", None, None);
    assert_eq!(
        merged,
        FooConfig {
            a: Some(9),
            ..Default::default()
        }
    );

    // Root without intercept yields the base config.
    let merged = root.resolve_config::<FooConfig>(
        "foo",
        Some(&FooConfig {
            a: Some(5),
            ..Default::default()
        }),
        None,
    );
    assert_eq!(
        merged,
        FooConfig {
            a: Some(5),
            ..Default::default()
        }
    );
}

#[tokio::test]
async fn extend_carries_metadata() {
    let root = Context::new();
    let child = root.extend(&[(
        "loader/entry-init",
        Rc::new(MetaValue("demo".into())) as Rc<dyn std::any::Any>,
    )]);

    let meta = child
        .meta::<MetaValue>("loader/entry-init")
        .expect("meta must exist");
    assert_eq!(meta.0, "demo");
    // Parent does not see the child's metadata.
    assert!(root.meta::<MetaValue>("loader/entry-init").is_none());
}
