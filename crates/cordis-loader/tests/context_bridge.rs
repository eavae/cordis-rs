//! The SDK Context surface across FFI.
//!
//! Covers the five vtable entries (`provide`/`get`/`on`/`emit`/
//! `effect_disposer`) end to end through the loader, plus entry-level
//! isolate visibility and the host-thread discipline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;

use cordis_core::{Context, FiberState};
use cordis_loader::{EntryOptions, IsolateValue, Loader, SoPlugin, context_bridge};
use libloading::Library;

static LOGGED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Serializes tests that share the fixture's process-global state.
static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

fn fixture_path() -> PathBuf {
    let mut path = std::env::current_dir().unwrap();
    path.push("..");
    path.push("..");
    path.push("target");
    path.push("debug");
    #[cfg(target_os = "macos")]
    let file = "libcordis_fixture_context.dylib";
    #[cfg(target_os = "linux")]
    let file = "libcordis_fixture_context.so";
    path.push(file);
    path
}

extern "C" fn log_message(message: *const std::ffi::c_char) {
    // SAFETY: the plugin passes a NUL-terminated string.
    let text = unsafe { std::ffi::CStr::from_ptr(message) }
        .to_string_lossy()
        .to_string();
    LOGGED.lock().unwrap().push(text);
}

fn opts(greeting: &str) -> EntryOptions {
    EntryOptions {
        id: String::new(),
        name: "cordis-context".to_string(),
        config: Some(
            serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&format!("greeting: {greeting}"))
                .unwrap(),
        ),
        group: None,
        disabled: None,
        inject: None,
        isolate: None,
        intercept: None,
        extra: Default::default(),
    }
}

fn isolate_map(entries: &[(&str, IsolateValue)]) -> HashMap<String, IsolateValue> {
    entries
        .iter()
        .map(|(name, value)| (name.to_string(), value.clone()))
        .collect()
}

fn fixture_helpers() -> (u32, u32, i32, i32) {
    // SAFETY: the fixture library is already loaded by the process.
    let library = unsafe { Library::new(fixture_path()) }.unwrap();
    type Count = unsafe extern "C" fn() -> u32;
    type Order = unsafe extern "C" fn() -> i32;
    // SAFETY: symbols exported by the fixture.
    let apply: libloading::Symbol<Count> = unsafe { library.get(b"plugin_apply_count") }.unwrap();
    let events: libloading::Symbol<Count> = unsafe { library.get(b"plugin_event_count") }.unwrap();
    let first: libloading::Symbol<Order> =
        unsafe { library.get(b"plugin_disposer_order_first") }.unwrap();
    let second: libloading::Symbol<Order> =
        unsafe { library.get(b"plugin_disposer_order_second") }.unwrap();
    (
        unsafe { apply() },
        unsafe { events() },
        unsafe { first() },
        unsafe { second() },
    )
}

/// Resets the fixture's process-global counters (shared across tests).
fn reset_fixture() {
    // SAFETY: the fixture library is already loaded by the process.
    let library = unsafe { Library::new(fixture_path()) }.unwrap();
    type Reset = unsafe extern "C" fn();
    // SAFETY: symbol exported by the fixture.
    let reset: libloading::Symbol<Reset> = unsafe { library.get(b"plugin_reset") }.unwrap();
    unsafe { reset() };
}

/// The plugin's apply provides a service the host reads back, responds to a
/// host-emitted event, and returns disposers that run in reverse
/// registration order on dispose.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn provide_get_event_and_disposers() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    reset_fixture();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());
            loader.register_so_plugin(&plugin).expect("register");

            LOGGED.lock().unwrap().clear();
            let tree = loader.tree_handle();
            let entry = tree.create(opts("hi"), None, 0);
            tree.await_tree().await;
            let fiber = entry.fiber.borrow().clone().expect("fiber created");
            assert_eq!(fiber.state.get(), FiberState::Active);

            // Host reads the service the plugin provided in apply.
            let greeting = entry
                .ctx
                .borrow()
                .get_str("greeting")
                .expect("plugin must provide greeting")
                .downcast_ref::<serde_yaml_ng::Value>()
                .cloned()
                .expect("greeting is a serde value");
            assert_eq!(greeting.as_str(), Some("hi"));
            let logged = LOGGED.lock().unwrap().clone();
            assert!(
                logged
                    .iter()
                    .any(|line| line.contains("context provided greeting: hi")),
                "apply must provide through the bridge: {logged:?}"
            );

            // Host emits an event; the plugin listener responds and writes
            // back through the vtable logger, including a `get` round-trip
            // (the fiber is ACTIVE during dispatch).
            let args: Rc<dyn std::any::Any> =
                Rc::new(serde_yaml_ng::Value::String("world".to_string()));
            entry.ctx.borrow().emit("demo/event", &[args]);
            let logged = LOGGED.lock().unwrap().clone();
            assert!(
                logged
                    .iter()
                    .any(|line| line.contains("context event fired: [\"world\"]")),
                "listener must run with the event args: {logged:?}"
            );
            assert!(
                logged
                    .iter()
                    .any(|line| line.contains("context get greeting: \"hi\"")),
                "get must round-trip the value during event dispatch: {logged:?}"
            );
            assert_eq!(fixture_helpers().1, 1, "one event callback");

            // Disposing the fiber runs the disposers in reverse
            // registration order (the second-registered one runs first).
            let _ = tokio::task::spawn_local(fiber.dispose()).await;
            for _ in 0..100 {
                if fixture_helpers().3 != -1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let (_, _, first_order, second_order) = fixture_helpers();
            assert_eq!(second_order, 0, "second-registered disposer runs first");
            assert_eq!(first_order, 1, "first-registered disposer runs second");
            let logged = LOGGED.lock().unwrap().clone();
            assert!(
                logged
                    .iter()
                    .any(|line| line.contains("context disposer second (order 0)")),
                "{logged:?}"
            );
            assert!(
                logged
                    .iter()
                    .any(|line| line.contains("context disposer first (order 1)")),
                "{logged:?}"
            );
        })
        .await;
}

/// Entry-level isolate scopes the `.so` plugin's provide/get the same way it
/// scopes core plugins; an intercepted entry still applies.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn isolate_scopes_provide_get() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    reset_fixture();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());
            loader.register_so_plugin(&plugin).expect("register");

            let tree = loader.tree_handle();
            let mut isolated = opts("A");
            isolated.id = "iso".to_string();
            isolated.isolate = Some(isolate_map(&[("greeting", IsolateValue::Flag(true))]));
            let isolated_entry = tree.create(isolated, None, 0);

            // The shared entry also carries an intercept declaration; it must
            // not disturb provide/get (the loader keeps intercept layers
            // inert until typed configs fill them).
            let mut shared = opts("B");
            shared.id = "shared".to_string();
            shared.intercept = Some(
                serde_yaml_ng::from_str::<serde_yaml_ng::Value>("{logger: {level: debug}}")
                    .unwrap(),
            );
            let shared_entry = tree.create(shared, None, 0);
            tree.await_tree().await;

            // The isolated realm sees only its own greeting.
            let isolated_greeting = isolated_entry
                .ctx
                .borrow()
                .get_str("greeting")
                .expect("isolated entry must see its own greeting")
                .downcast_ref::<serde_yaml_ng::Value>()
                .cloned()
                .unwrap();
            assert_eq!(isolated_greeting.as_str(), Some("A"));

            // The shared realm sees the shared greeting, not the isolated one.
            let shared_greeting = shared_entry
                .ctx
                .borrow()
                .get_str("greeting")
                .expect("shared entry must see the shared greeting")
                .downcast_ref::<serde_yaml_ng::Value>()
                .cloned()
                .unwrap();
            assert_eq!(shared_greeting.as_str(), Some("B"));

            // Root (no isolate label for greeting) sees only the shared one.
            let root_greeting = root
                .get_str("greeting")
                .expect("root must see the shared greeting")
                .downcast_ref::<serde_yaml_ng::Value>()
                .cloned()
                .unwrap();
            assert_eq!(root_greeting.as_str(), Some("B"));

            // The isolated entry's fiber resolves the same label as its
            // entry context (the .so plugin provided under the isolated
            // label, not the root one).
            let isolated_fiber = isolated_entry.fiber.borrow().clone().unwrap();
            let isolated_label = isolated_entry.ctx.borrow().isolate_label("greeting");
            let fiber_label = isolated_fiber.context().isolate_label("greeting");
            assert_eq!(isolated_label, fiber_label);
            assert_ne!(
                root.isolate_label("greeting"),
                isolated_label,
                "isolated greeting must use a realm-local label"
            );
            let _ = handle;
        })
        .await;
}

/// The vtable only resolves sessions on the host thread. Calls from a
/// foreign thread fail gracefully (no session), and a disposed plugin's
/// deferred callbacks are skipped instead of calling into freed code.
#[test]
fn host_thread_discipline_and_live_handle_guard() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    reset_fixture();
    let root = Context::new();
    let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
    let handle = unsafe { plugin.create(log_message) };
    assert!(!handle.is_null());
    assert!(context_bridge::is_handle_live(handle));

    // A foreign thread has no session and no live handle registry: the
    // bridge refuses the call without panicking.
    let name = std::ffi::CString::new("greeting").unwrap();
    let payload = std::ffi::CString::new("\"x\"").unwrap();
    let handle_token = handle as usize;
    let foreign = std::thread::spawn(move || {
        let handle = handle_token as *mut cordis_sdk::PluginHandle;
        let provided =
            unsafe { context_bridge::host_provide(handle, name.as_ptr(), payload.as_ptr()) };
        let got = unsafe { context_bridge::host_get(handle, name.as_ptr()) };
        (
            provided,
            got.is_null(),
            context_bridge::is_handle_live(handle),
        )
    });
    let (provided, got_null, live) = foreign.join().unwrap();
    assert_eq!(provided, 1, "foreign provide must fail");
    assert!(got_null, "foreign get must return null");
    assert!(!live, "live registry is host-thread scoped");

    // On the host thread a session resolves: provide then get round-trip.
    context_bridge::with_session(handle, &root, || {
        let name = std::ffi::CString::new("greeting").unwrap();
        let payload = std::ffi::CString::new("\"host session\"").unwrap();
        let provided =
            unsafe { context_bridge::host_provide(handle, name.as_ptr(), payload.as_ptr()) };
        assert_eq!(provided, 0, "host-thread provide must succeed");
        let got = unsafe { context_bridge::host_get(handle, name.as_ptr()) };
        assert!(!got.is_null(), "host-thread get must resolve");
        // SAFETY: the pointer is the session scratch for this call.
        let text = unsafe { std::ffi::CStr::from_ptr(got) }.to_string_lossy();
        assert_eq!(text, "\"host session\"");
    });

    drop(plugin);
    assert!(!context_bridge::is_handle_live(handle));
}

/// Deferred path: an event callback registered by a plugin instance that is
/// disposed while its fiber is still alive is skipped safely.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn disposed_plugin_event_callback_is_skipped() {
    let _guard = FIXTURE_LOCK.lock().unwrap();
    reset_fixture();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let root = Context::new();
            let loader = Loader::new(&root);
            let mut plugin = unsafe { SoPlugin::load(&fixture_path()) }.unwrap();
            let handle = unsafe { plugin.create(log_message) };
            assert!(!handle.is_null());
            loader.register_so_plugin(&plugin).expect("register");

            LOGGED.lock().unwrap().clear();
            let tree = loader.tree_handle();
            let entry = tree.create(opts("hi"), None, 0);
            tree.await_tree().await;

            // Drop the plugin instance while the fiber (and its listener)
            // are still alive; the host must skip the deferred callback.
            drop(plugin);
            entry.ctx.borrow().emit("demo/event", &[]);
            assert_eq!(fixture_helpers().1, 0, "callback must be skipped");

            // Disposing the fiber runs the disposers, which are skipped the
            // same way (the handle is no longer live).
            let fiber = entry.fiber.borrow().clone().unwrap();
            let _ = tokio::task::spawn_local(fiber.dispose()).await;
            let (_, _, first_order, second_order) = fixture_helpers();
            assert_eq!(first_order, -1, "disposer must be skipped");
            assert_eq!(second_order, -1, "disposer must be skipped");
            let _ = handle;
        })
        .await;
}
