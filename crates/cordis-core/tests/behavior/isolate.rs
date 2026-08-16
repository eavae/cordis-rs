//! Ported cases from `packages/core/tests/isolate.spec.ts` (story card B1,
//! provider/visibility parts; plugin & event parts land in B4/B5/B8).

use std::rc::Rc;

use cordis_core::{Context, Service};

#[derive(Debug)]
struct Foo {
    bar: i32,
}

impl Service for Foo {
    const NAME: &'static str = "foo";
}

#[tokio::test]
async fn isolation_isolated_context() {
    let root = Context::new();
    let ctx1 = root.isolate("foo", Rc::from("symbol-1"));
    let ctx2 = root.isolate("foo", Rc::from("symbol-2"));

    let dispose0 = root.provide::<Foo>(Rc::new(Foo { bar: 100 })).unwrap();
    assert_eq!(root.get::<Foo>().unwrap().bar, 100);
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
async fn isolation_shared_label() {
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
