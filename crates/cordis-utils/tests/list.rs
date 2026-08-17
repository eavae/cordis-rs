//! Utils List 工具。

use std::rc::Rc;

use cordis_core::Context;
use cordis_utils::List;

#[tokio::test(flavor = "current_thread")]
async fn push_grows_and_dispose_removes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let list = List::new();
            assert_eq!(list.len(), 0);

            let handle = list.push(&root, 1).unwrap();
            list.push(&root, 2).unwrap();
            assert_eq!(list.len(), 2);

            handle.dispose().await.unwrap();
            assert_eq!(list.len(), 1);
            assert_eq!(*list.iter()[0], 2);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn context_dispose_removes_items() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let list = List::new();
            let list_apply = list.clone();
            let fiber = root.plugin(
                &cordis_core::Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Rc::new(move |ctx: &Context, _config| {
                        drop(list_apply.push(ctx, "hello").unwrap());
                        cordis_core::Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(list.len(), 1);

            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(list.len(), 0, "items must be removed on context dispose");
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn filter_and_map() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let list = List::new();
            for value in 1..=5 {
                list.push(&root, value).unwrap();
            }
            let evens = list.filter(|value| value % 2 == 0);
            assert_eq!(evens.iter().map(|v| **v).collect::<Vec<_>>(), vec![2, 4]);

            let doubled = list.map(|value| value * 2);
            assert_eq!(doubled, vec![2, 4, 6, 8, 10]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn debug_output() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let list = List::new();
            list.push(&root, 1).unwrap();
            list.push(&root, 2).unwrap();
            assert_eq!(format!("{list:?}"), "[1, 2]");
        })
        .await;
}
