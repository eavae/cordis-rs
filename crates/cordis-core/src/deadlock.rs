//! Deadlock detection for dev/test builds.
//!
//! Enabled by the `deadlock-detection` cargo feature, which turns on
//! parking_lot's `deadlock_detection`. The runtime lock graph is acyclic by
//! design (see the lock-ordering docs on [`crate::Fiber`] and
//! [`crate::registry::RegistryService`]); the watchdog here is defense in
//! depth: if a lock cycle ever forms, the involved threads' backtraces are
//! printed and the process exits with an error instead of hanging tests
//! forever.

/// Installs the deadlock watchdog, if the `deadlock-detection` feature is
/// enabled.
///
/// The watchdog runs once per process: it periodically inspects parking_lot's
/// lock registry for cycles and reports the affected threads' backtraces
/// before exiting with an error. [`crate::Context::new`] calls this
/// automatically, so test binaries get the watchdog without any setup;
/// embedders may call it explicitly as well. Without the feature this is a
/// no-op.
pub fn install() {
    #[cfg(feature = "deadlock-detection")]
    install_watchdog();
}

#[cfg(feature = "deadlock-detection")]
fn install_watchdog() {
    use std::sync::OnceLock;
    use std::time::Duration;

    static WATCHDOG: OnceLock<()> = OnceLock::new();
    let _ = WATCHDOG.get_or_init(|| {
        std::thread::Builder::new()
            .name("deadlock-watchdog".to_string())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(3));
                    let deadlocks = parking_lot::deadlock::check_deadlock();
                    if deadlocks.is_empty() {
                        continue;
                    }
                    for (index, threads) in deadlocks.iter().enumerate() {
                        eprintln!("==== deadlock detected (cycle {index}) ====");
                        for thread in threads {
                            eprintln!("thread id {:#?}", thread.thread_id());
                            eprintln!("{:#?}", thread.backtrace());
                        }
                    }
                    std::process::exit(1);
                }
            })
            .expect("failed to spawn deadlock watchdog");
    });
}
