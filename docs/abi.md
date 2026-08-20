# `.so` Plugin ABI Protocol

This document defines the cross-library protocol between the host
(`cordis-loader`) and `.so` plugins (`cordis-sdk`): entry symbols, versioning,
vtable layout, handle lifecycle, allocator discipline and error conventions.

> 中文版见 [docs/abi_cn.md](abi_cn.md) · English version: this file.

## 1. Entry symbols and versioning

Every plugin cdylib must export the following symbols. The host validates each
one at load time and rejects the plugin when a symbol is missing or the ABI
version mismatches:

| Symbol | Signature | Description |
| --- | --- | --- |
| `plugin_api_version` | `fn() -> u32` | The ABI version the plugin implements; must equal the host's `PLUGIN_API_VERSION` |
| `plugin_create` | `fn(*const HostVtable) -> *mut PluginHandle` | Creates a plugin instance; returns null on version mismatch |
| `plugin_dispose` | `fn(*mut PluginHandle)` | Destroys a plugin instance |
| `plugin_meta` | `fn() -> *const c_char` | (optional) Metadata JSON: `name`/`version`/`inject`/`provide`/`deps` |
| `plugin_validate_config` | `fn(*const c_char) -> i32` | (optional) Config validation; 0 = valid, non-zero = rejected |
| `plugin_apply` | `fn(*mut PluginHandle, *const c_char) -> i32` | (optional) Applies a config; runs inside a host session |

Current version: `PLUGIN_API_VERSION = 3`.

- v2: entry protocol + async bridge (`log`/`spawn`).
- v3: the vtable gains five Context bridge entries:
  `provide`/`get`/`on`/`emit`/`effect_disposer`.

`deps` declares the host crates/services the plugin links against; the HMR
plugin uses it for dependency classification.

## 2. Host vtable layout

`HostVtable` is a `#[repr(C)]` struct passed by the host at `plugin_create`;
the plugin holds its pointer for the instance's lifetime. All function
pointers are only invoked on the host thread (single-thread discipline, §6).

```rust
pub struct HostVtable {
    pub log: extern "C" fn(message: *const c_char),          // logging
    pub spawn: HostSpawn,                                    // async bridge
    pub provide: HostProvide,                                // register a service
    pub get: HostGet,                                        // read a service
    pub on: HostOn,                                          // register an event listener
    pub emit: HostEmit,                                      // emit an event
    pub effect_disposer: HostEffectDisposer,                 // register a fiber disposer
    pub data: *mut c_void,                                   // host runtime handle
    pub host_version: u32,                                   // host ABI version
}
```

### Context bridge entry semantics

| vtable entry | Signature | Semantics |
| --- | --- | --- |
| `provide` | `fn(handle, name, payload_json) -> i32` | Registers a service on the plugin's current fiber; `payload_json` is a JSON value; 0 = ok, non-zero = failed (duplicate registration / no session) |
| `get` | `fn(handle, name) -> *const c_char` | Reads a service back as a JSON string; null when missing or not serializable |
| `on` | `fn(handle, event, callback) -> *mut c_void` | Registers a listener; returns an opaque host-owned listener handle; removed automatically when the fiber unloads |
| `emit` | `fn(handle, event, payload_json)` | Emits an event; `payload_json` must be a JSON array of arguments |
| `effect_disposer` | `fn(handle, disposer_fn)` | Registers a disposer on the plugin's current fiber; runs in reverse registration order on unload |

Plugins call these entries through the SDK's `ContextBridge`:

```rust
use cordis_sdk::{ContextBridge, HostVtable, PluginHandle};

unsafe extern "C" fn plugin_apply(handle: *mut PluginHandle, config: *const c_char) -> i32 {
    let vtable = /* host vtable saved at plugin_create */;
    // SAFETY: the current call runs inside a host session.
    let bridge = unsafe { ContextBridge::new(vtable, handle) };
    bridge.provide("greeting", "\"hello\"").unwrap();
    let _ = bridge.get("greeting");                    // Some("\"hello\"")
    bridge.on("demo/event", on_demo_event).unwrap();
    bridge.effect_disposer(disposer);
    bridge.emit("demo/event", "[\"hi\"]");
    0
}
```

## 3. Session model (handle → Context)

Before calling into the plugin (apply, event listener, or disposer), the host
pushes a **session** that binds the plugin `handle` to the current fiber's
`Context`. Each vtable entry looks up the innermost session matching the
passed `handle`, then executes `provide`/`get`/`on`/`emit`/`effect_disposer`
on that `Context`.

Key points:

- A single `.so` instance may be shared by multiple fibers (multi-instance
  fixture); sessions distinguish "handle + call time", so services
  registered by one fiber never leak into another.
- Sessions nest: when a plugin event callback calls `emit` or registers a
  disposer, a new session is pushed and popped on return.
- Sessions only exist on the host thread; vtable calls from other threads
  fail silently (`provide` returns non-zero, `get` returns null, `on` returns
  null).
- **Async limitation**: tasks spawned via `spawn` run on the host runtime
  but outside any session; plugins must not call
  `provide`/`get`/`on`/`emit`/`effect_disposer` from spawned async code.
  Copy values out with `get` during apply/callbacks, or register listeners in
  advance.

## 4. Handle lifecycle

- After `plugin_create` succeeds, the host's `SoPlugin` owns the handle;
  `SoPlugin::drop` calls `plugin_dispose` and unregisters the handle.
- The host keeps a live-handle registry. Deferred callbacks (event listeners,
  disposers) check liveness before invoking plugin code: if the instance has
  been disposed while its fiber is still unloading, the callback is skipped
  (an error is logged) and freed plugin code is never called.
- The listener handle returned by `on` is an opaque host-owned pointer for
  identification only; plugins must not dereference it. This version has no
  `off`; listeners are removed automatically when the fiber unloads.

## 5. Allocator discipline and value passing

**Allocation never crosses the boundary**.

- Cross-boundary values are carried as JSON strings; the caller owns the
  string during the call and the host copies and parses it immediately.
- The `get` result points into a per-session scratch buffer on the host; it
  is only valid until the next host call into the same session. Plugins must
  copy it right away (the SDK's `ContextBridge::get` already does).
- Event arguments are serialized by the host into a JSON array;
  non-serializable arguments (`Rc<dyn Any>` objects) are encoded as `null`.
- Non-JSON object services (e.g. host-side Rust services) cannot cross the
  boundary; `get` returns null. Plugins should declare dependencies via the
  metadata `inject` field and let the host resolve them.

## 6. Single-thread discipline

All vtable calls happen only on the host's current-thread runtime (core
decision 3):

- Compile time: core/SDK contexts are `!Send` (`Rc`/`RefCell`); `HostVtable`
  is `Send`/`Sync` only because of FFI pointers.
- Runtime: the session registry is `thread_local`; calls from other threads
  find no session and fail gracefully instead of panicking.
- Plugins must not bring their own runtime; all async goes through the
  SDK's `spawn(vtable, future)` to the host.

## 7. Error conventions

- `plugin_create`: returns null on version mismatch or invalid vtable.
- `plugin_validate_config` / `plugin_apply`: 0 = success, non-zero = failure.
- `provide` / `on`: a non-zero/null return means failure (duplicate
  registration, no session, disposed fiber).
- `get` / `emit`: null / silent ignore on failure; the host logs diagnostics
  through its logger.

## 8. SDK public surface (`cordis::Context` shape inside `.so`)

`.so` plugins cannot hold the core `Context` directly; the equivalent is the
vtable plus `ContextBridge`:

| core API | `.so` equivalent | Notes |
| --- | --- | --- |
| `ctx.provide(name, value)` | `bridge.provide(name, json)` | The value must be JSON-serializable data |
| `ctx.get(name)` | `bridge.get(name) -> Option<String>` | Only JSON data services come back |
| `ctx.on(event, cb)` | `bridge.on(event, callback)` | Callback signature `fn(handle, args_json)` |
| `ctx.emit(event, ...args)` | `bridge.emit(event, args_json)` | Arguments are a JSON array |
| `Effect::Disposer(d)` | `bridge.effect_disposer(fn)` | Runs in reverse registration order on unload |
| `ctx.logger()` | vtable `log` | String-only logging, no structured logger |
| `tokio::task::spawn_local` | SDK `spawn(vtable, future)` | The host runtime drives the task |

Limitations:

- Object services cannot cross the boundary or be invoked from plugins; only
  data services and events are supported.
- During apply the fiber is `LOADING`; strict service lookup (`get`) requires
  the provider fiber to be `ACTIVE`, so reading a value provided in the same
  apply fails. Read values once the fiber is `ACTIVE` (e.g. inside an event
  callback; see the Context bridge fixture).
- Listeners/disposers are bound to the fiber lifecycle and cannot be removed
  manually (`off` is planned for a later version).
