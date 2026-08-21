//! Real-thread contention tests for the lock-free snapshot writes.
//!
//! `loom` cannot model `arc_swap`, so these tests use real threads to verify
//! that the compare-and-swap loops never lose updates and conditional writes
//! keep their single-winner semantics.

use std::sync::Arc;

use cordis_core::{Context, Label};

#[test]
fn concurrent_provides_never_lose_updates() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 40;
    let root = Arc::new(Context::new());
    let mut handles = Vec::new();
    for t in 0..THREADS {
        let root = root.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..PER_THREAD {
                let name = format!("s{t}-{i}");
                root.provide_str(&name, Arc::new(t * PER_THREAD + i))
                    .unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            let name = format!("s{t}-{i}");
            assert!(
                root.get_str_non_strict(&name).is_some(),
                "lost update for {name}"
            );
        }
    }
}

#[test]
fn concurrent_label_migration_has_single_winner() {
    const THREADS: usize = 8;
    let root = Arc::new(Context::new());
    root.provide_str("svc", Arc::new(1i32)).unwrap();
    let old_label = root.isolate_label("svc").expect("label");
    let new_label: Label = Arc::from("migrated#1");
    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let root = root.clone();
        let old_label = old_label.clone();
        let new_label = new_label.clone();
        handles.push(std::thread::spawn(move || {
            root.migrate_label_if("svc", &old_label, &new_label, root.fiber())
        }));
    }
    let results: Vec<bool> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(
        results.iter().filter(|won| **won).count(),
        1,
        "exactly one migration must win"
    );
    // The entry moved out of the old realm: it is no longer visible under
    // the old label, but resolves again once the overlay follows it.
    assert!(root.get_str_non_strict("svc").is_none());
    root.set_isolate("svc", new_label);
    assert!(root.get_str_non_strict("svc").is_some());
}
