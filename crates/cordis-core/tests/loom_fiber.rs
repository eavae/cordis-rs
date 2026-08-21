//! Loom model checks for the `Fiber` state machine.
//!
//! The production `Fiber` stores its lifecycle state in an `AtomicU8`
//! (`SeqCst`) and its error in a `Mutex`. The ordering contract we rely on:
//! a fiber that observes `Failed` must also observe the error that was
//! written before the transition. These tests model that contract with the
//! same memory orders (`SeqCst`) and let `loom` explore every interleaving.

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// The `FiberState::Failed` discriminant (mirrors `fiber.rs`).
const FAILED: u8 = 3;

/// The production discipline: write the error first, then publish the
/// `Failed` state. `SeqCst` ordering then makes the error visible to any
/// reader that observes the transition.
#[test]
fn failed_state_is_ordered_before_error_visibility() {
    loom::model(|| {
        let state = Arc::new(AtomicU8::new(0));
        let error_flag = Arc::new(AtomicBool::new(false));
        let observed = Arc::new((AtomicBool::new(false), AtomicBool::new(false)));

        let state_w = state.clone();
        let flag_w = error_flag.clone();
        let writer = loom::thread::spawn(move || {
            flag_w.store(true, Ordering::SeqCst);
            state_w.store(FAILED, Ordering::SeqCst);
        });

        let observed_r = observed.clone();
        let reader = loom::thread::spawn(move || {
            if state.load(Ordering::SeqCst) == FAILED {
                observed_r.0.store(true, Ordering::SeqCst);
                if error_flag.load(Ordering::SeqCst) {
                    observed_r.1.store(true, Ordering::SeqCst);
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();

        let saw_failed = observed.0.load(Ordering::SeqCst);
        let saw_error = observed.1.load(Ordering::SeqCst);
        assert!(
            !saw_failed || saw_error,
            "a reader that sees Failed must also see the error flag"
        );
    });
}

/// The same rule for the active transition: the epoch payload is written
/// before the state is published (mirrors `set_epoch`, which replaces the
/// epoch before `update_state` broadcasts the transition).
#[test]
fn active_state_is_ordered_after_epoch_payload() {
    loom::model(|| {
        // State `Active` discriminant (mirrors `fiber.rs`).
        const ACTIVE: u8 = 2;
        let state = Arc::new(AtomicU8::new(0));
        let epoch_payload = Arc::new(AtomicBool::new(false));
        let observed = Arc::new((AtomicBool::new(false), AtomicBool::new(false)));

        let state_w = state.clone();
        let payload_w = epoch_payload.clone();
        let writer = loom::thread::spawn(move || {
            payload_w.store(true, Ordering::SeqCst);
            state_w.store(ACTIVE, Ordering::SeqCst);
        });

        let observed_r = observed.clone();
        let reader = loom::thread::spawn(move || {
            if state.load(Ordering::SeqCst) == ACTIVE {
                observed_r.0.store(true, Ordering::SeqCst);
                if epoch_payload.load(Ordering::SeqCst) {
                    observed_r.1.store(true, Ordering::SeqCst);
                }
            }
        });

        writer.join().unwrap();
        reader.join().unwrap();

        let saw_active = observed.0.load(Ordering::SeqCst);
        let saw_payload = observed.1.load(Ordering::SeqCst);
        assert!(
            !saw_active || saw_payload,
            "observing Active implies observing the epoch payload"
        );
    });
}
