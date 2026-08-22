//! Ported cases from `packages/core/tests/dispose.spec.ts`.

use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context as TaskContext, Poll};

use cordis_core::{
    AsyncDisposerStream, Context, Effect, EffectItem, EffectMeta, EventOptions, Plugin,
    async_disposer, event_listener, sync_disposer,
};

use super::Timers;

#[derive(Clone)]
struct Seq {
    values: Arc<Mutex<Vec<i32>>>,
}

impl Seq {
    fn new() -> Self {
        Self {
            values: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push(&self, value: i32) {
        self.values.lock().push(value);
    }

    fn get(&self) -> Vec<i32> {
        self.values.lock().clone()
    }
}

/// An async generator equivalent used by the `async yield` cases: every step
/// sleeps 100ms, pushes `2*step+1`, then yields a disposer that pushes
/// `2*(step+1)`.
struct YieldStream {
    timers: Timers,
    seq: Seq,
    step: usize,
    pending: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl AsyncDisposerStream for YieldStream {
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<cordis_core::Disposer, Box<dyn std::error::Error + Send + Sync>>>> {
        match self.step {
            0..=2 => {
                if self.pending.is_none() {
                    let timers = self.timers.clone();
                    let seq = self.seq.clone();
                    let step = self.step;
                    self.pending = Some(Box::pin(async move {
                        timers.sleep(100).await;
                        seq.push((step as i32) * 2 + 1);
                    }));
                }
                let pending = self.pending.as_mut().expect("pending task");
                match pending.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.pending = None;
                        let step = self.step;
                        self.step += 1;
                        let seq = self.seq.clone();
                        Poll::Ready(Some(Ok(Box::new(move || {
                            let seq = seq.clone();
                            Box::pin(async move {
                                seq.push((step as i32 + 1) * 2);
                                Ok(())
                            })
                        }))))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            _ => Poll::Ready(None),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn effects_dispose_by_plugin() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let dispose_called = Arc::new(AtomicU32::new(0));
            let dispose_called_apply = dispose_called.clone();
            let fiber = root.plugin(
                &Plugin {
                    is_group: false,
                    name: None,
                    inject: Vec::new(),
                    apply: Arc::new(move |ctx: &Context, _config| {
                        let dispose_called = dispose_called_apply.clone();
                        ctx.effect(
                            || {
                                Effect::Disposer(sync_disposer(move || {
                                    dispose_called.store(
                                        dispose_called.load(Ordering::SeqCst) + 1,
                                        Ordering::SeqCst,
                                    );
                                }))
                            },
                            "test",
                        )
                        .unwrap();
                        Effect::None
                    }),
                },
                None,
            );
            fiber.wait().await.unwrap();
            assert_eq!(
                fiber.get_effects(),
                vec![EffectMeta {
                    label: "test".to_string(),
                    children: Vec::new(),
                }]
            );
            assert_eq!(dispose_called.load(Ordering::SeqCst), 0);

            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(dispose_called.load(Ordering::SeqCst), 1);

            fiber.dispose().await;
            tokio::task::yield_now().await;
            assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_dispose_manually() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let dispose_called = Arc::new(AtomicU32::new(0));
            let handle = {
                let dispose_called = dispose_called.clone();
                root.effect(
                    || {
                        Effect::Disposer(sync_disposer(move || {
                            dispose_called
                                .store(dispose_called.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            assert_eq!(
                root.fiber().get_effects(),
                vec![EffectMeta {
                    label: "anonymous".to_string(),
                    children: Vec::new(),
                }]
            );
            assert_eq!(dispose_called.load(Ordering::SeqCst), 0);

            handle.dispose().await.unwrap();
            assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
            handle.dispose().await.unwrap();
            assert_eq!(dispose_called.load(Ordering::SeqCst), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_yield_dispose() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let seq = Seq::new();
            let on_effect = root
                .on(
                    "custom-event",
                    event_listener(|_| {}),
                    EventOptions::default(),
                )
                .unwrap();
            let nested = {
                let seq = seq.clone();
                let on_effect = root
                    .on(
                        "custom-event",
                        event_listener(|_| {}),
                        EventOptions::default(),
                    )
                    .unwrap();
                let seq2 = seq;
                root.effect(
                    move || {
                        Effect::Iterable(vec![
                            Ok(EffectItem::Nested(on_effect.clone())),
                            Ok(EffectItem::Disposer(sync_disposer(move || {
                                seq2.push(3);
                            }))),
                        ])
                    },
                    "anonymous",
                )
                .unwrap()
            };
            let handle = {
                let seq1 = seq.clone();
                let seq2 = seq.clone();
                root.effect(
                    move || {
                        Effect::Iterable(vec![
                            Ok(EffectItem::Disposer(sync_disposer(move || {
                                seq1.push(1);
                            }))),
                            Ok(EffectItem::Nested(on_effect.clone())),
                            Ok(EffectItem::Disposer(sync_disposer(move || {
                                seq2.push(2);
                            }))),
                            Ok(EffectItem::Nested(nested)),
                        ])
                    },
                    "anonymous",
                )
                .unwrap()
            };
            drop(
                root.on(
                    "custom-event",
                    event_listener(|_| {}),
                    EventOptions::default(),
                )
                .unwrap(),
            );

            // Root-level metadata only includes the outer anonymous effect
            // and the standalone listener; nested effects are owned by their
            // parent.
            let root_effects = root.fiber().get_effects();
            assert_eq!(root_effects.len(), 2);
            assert_eq!(root_effects[0].label, "anonymous");
            assert_eq!(root_effects[0].children.len(), 2);
            assert_eq!(
                root_effects[0].children[0].label,
                "ctx.on(\"custom-event\")"
            );
            assert_eq!(root_effects[0].children[1].label, "anonymous");
            assert_eq!(
                root_effects[0].children[1].children[0].label,
                "ctx.on(\"custom-event\")"
            );
            assert_eq!(root_effects[1].label, "ctx.on(\"custom-event\")");

            assert_eq!(seq.get(), Vec::<i32>::new());
            handle.dispose().await.unwrap();
            assert_eq!(seq.get(), vec![3, 2, 1]);
            handle.dispose().await.unwrap();
            assert_eq!(seq.get(), vec![3, 2, 1]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_return_1() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                let seq = seq.clone();
                root.effect(
                    move || {
                        Effect::Async(Box::pin(async move {
                            timers.sleep(100).await;
                            seq.push(1);
                            let seq = seq.clone();
                            Ok(async_disposer(move || async move {
                                seq.push(2);
                                Ok(())
                            }))
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            // Let the background task register its timer at t=0.
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), Vec::<i32>::new());
            timers.advance(100).await;
            assert_eq!(seq.get(), vec![1]);
            handle.dispose().await.unwrap();
            assert_eq!(seq.get(), vec![1, 2]);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_return_2() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                let seq = seq.clone();
                root.effect(
                    move || {
                        Effect::Async(Box::pin(async move {
                            timers.sleep(100).await;
                            seq.push(1);
                            let seq = seq.clone();
                            Ok(async_disposer(move || async move {
                                seq.push(2);
                                Ok(())
                            }))
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            std::mem::drop(handle.dispose());
            assert_eq!(seq.get(), Vec::<i32>::new());
            timers.advance(100).await;
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), vec![1, 2]);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_yield_1() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                let seq = seq.clone();
                root.effect(
                    move || {
                        Effect::AsyncIterable(Box::pin(YieldStream {
                            timers: timers.clone(),
                            seq,
                            step: 0,
                            pending: None,
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), Vec::<i32>::new());
            timers.advance(300).await;
            assert_eq!(seq.get(), vec![1, 3, 5]);
            handle.dispose().await.unwrap();
            assert_eq!(seq.get(), vec![1, 3, 5, 6, 4, 2]);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_yield_2_aborted() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                let seq = seq.clone();
                root.effect(
                    move || {
                        Effect::AsyncIterable(Box::pin(YieldStream {
                            timers: timers.clone(),
                            seq,
                            step: 0,
                            pending: None,
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            timers.advance(50).await;
            std::mem::drop(handle.dispose());
            assert_eq!(seq.get(), Vec::<i32>::new());
            timers.advance(300).await;
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), vec![1, 2]);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_yield_3_aborted() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                let seq = seq.clone();
                root.effect(
                    move || {
                        Effect::AsyncIterable(Box::pin(YieldStream {
                            timers: timers.clone(),
                            seq,
                            step: 0,
                            pending: None,
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), Vec::<i32>::new());
            timers.advance(100).await;
            assert_eq!(seq.get(), vec![1]);
            std::mem::drop(handle.dispose());
            assert_eq!(seq.get(), vec![1]);
            timers.advance(200).await;
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), vec![1, 3, 4, 2]);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_yield_4_await_dispose() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                let seq = seq.clone();
                root.effect(
                    move || {
                        Effect::AsyncIterable(Box::pin(YieldStream {
                            timers: timers.clone(),
                            seq,
                            step: 0,
                            pending: None,
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), Vec::<i32>::new());
            timers.advance(300).await;
            assert_eq!(seq.get(), vec![1, 3, 5]);
            handle.dispose().await.unwrap();
            assert_eq!(seq.get(), vec![1, 3, 5, 6, 4, 2]);
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_return_with_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let seq = Seq::new();
            let error = root
                .fiber()
                .effect(
                    || Effect::Error(Box::new(std::io::Error::other("test"))),
                    "anonymous",
                )
                .unwrap_err();
            assert_eq!(error.message, "test");
            assert_eq!(seq.get(), Vec::<i32>::new());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_yield_with_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let seq = Seq::new();
            let error = {
                let seq = seq.clone();
                root.fiber()
                    .effect(
                        move || {
                            Effect::Iterable(vec![
                                Ok(EffectItem::Disposer(sync_disposer(move || {
                                    seq.push(1);
                                }))),
                                Err(Box::new(std::io::Error::other("test"))),
                            ])
                        },
                        "anonymous",
                    )
                    .unwrap_err()
            };
            assert_eq!(error.message, "test");
            // The disposer yielded before the error is cleaned up.
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), vec![1]);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_return_with_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let timers = timers.clone();
                root.effect(
                    move || {
                        Effect::Async(Box::pin(async move {
                            timers.sleep(100).await;
                            Err::<cordis_core::Disposer, Box<dyn std::error::Error + Send + Sync>>(
                                Box::new(std::io::Error::other("test")),
                            )
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), Vec::<i32>::new());
            let dispose_future = handle.dispose();
            timers.advance(100).await;
            let result = dispose_future.await;
            assert!(result.is_err());
            assert_eq!(seq.get(), Vec::<i32>::new());
        }))
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn effects_async_yield_with_error() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(super::with_timers(|timers| async move {
            let root = Context::new();
            let seq = Seq::new();
            let handle = {
                let seq = seq.clone();
                let timers_for_stream = timers.clone();
                root.effect(
                    move || {
                        Effect::AsyncIterable(Box::pin(ErrorStream {
                            timers: timers_for_stream.clone(),
                            seq,
                            step: 0,
                            pending: None,
                        }))
                    },
                    "anonymous",
                )
                .unwrap()
            };
            tokio::task::yield_now().await;
            assert_eq!(seq.get(), Vec::<i32>::new());
            let wait_future = handle.wait_task();
            timers.advance(100).await;
            let result = wait_future.await;
            assert!(result.is_err());
            assert_eq!(seq.get(), vec![1]);
        }))
        .await;
}

/// Stream that yields one disposer (push 1) and then fails.
struct ErrorStream {
    timers: Timers,
    seq: Seq,
    step: usize,
    pending: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl AsyncDisposerStream for ErrorStream {
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<cordis_core::Disposer, Box<dyn std::error::Error + Send + Sync>>>> {
        match self.step {
            0 => {
                if self.pending.is_none() {
                    let timers = self.timers.clone();
                    self.pending = Some(Box::pin(async move {
                        timers.sleep(100).await;
                    }));
                }
                let pending = self.pending.as_mut().expect("pending task");
                match pending.as_mut().poll(cx) {
                    Poll::Ready(()) => {
                        self.pending = None;
                        self.step = 1;
                        let seq = self.seq.clone();
                        Poll::Ready(Some(Ok(Box::new(move || {
                            let seq = seq;
                            Box::pin(async move {
                                seq.push(1);
                                Ok(())
                            })
                        }))))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            _ => Poll::Ready(Some(Err(Box::new(std::io::Error::other("test"))))),
        }
    }
}
