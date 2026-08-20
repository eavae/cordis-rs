# Cordis (Rust Edition)

中文:[README_cn.md](README_cn.md) · English (this file)

An **unofficial Rust implementation** of [Cordis](https://github.com/cordisjs/cordis) — a meta-framework of spatiotemporal composability for plugin-based applications.

> This is a from-scratch port. It is not affiliated with or endorsed by the
> official Cordis project. Both upstream and this port are under active
> development; the API is not yet stable and may change without notice.

## Version alignment

The API surface and system design of this port follow upstream **`cordis` 4.0.0-rc.8** (the JS/TS package, `packages/core` in the upstream repo).

To make the follow-relationship obvious at a glance, every crate in this workspace shares a single version that is **kept identical to the upstream core package: currently `4.0.0-rc.8`**. When upstream releases a new version, this repo adapts and then bumps to the same number.

Crate-to-package mapping:

| Upstream package (npm) | Upstream version | Crate in this repo |
| --- | --- | --- |
| `cordis` | 4.0.0-rc.8 | `cordis` / `cordis-core` |
| `@cordisjs/plugin-loader` | 1.0.0-rc.5 | `cordis-loader` |
| `@cordisjs/plugin-group` | 1.0.0 | `cordis-plugin-group` |
| `@cordisjs/plugin-include` | 1.0.4 | `cordis-plugin-include` |
| `@cordisjs/plugin-hmr` | 1.0.15 | `cordis-plugin-hmr` |
| `@cordisjs/plugin-timer` | 1.1.2 | `cordis-plugin-timer` |
| `@cordisjs/plugin-logger-console` | 1.0.0 | `cordis-plugin-logger-console` |
| `@cordisjs/utils` | 1.0.0 | `cordis-utils` |
| `create-cordis` | 0.3.0 | `cordis-cli create` (local template) |

Independent of the crate version there is a **plugin ABI version**: `PLUGIN_API_VERSION = 3`. The host checks it for strict equality when loading a `.so` plugin and rejects mismatches. See [docs/abi.md](docs/abi.md) ([中文](docs/abi_cn.md)).

## Quick start

> This project is not published to crates.io yet; everything below works from
> a source checkout (Rust 1.97+ required).

A cordis application has two parts: a **host program** (app) and **plugins** (compiled as cdylib dynamic libraries). At runtime the host reads `cordis.yml` and scans the `plugins/` directory for `.so` / `.dylib` files.

### 1. Scaffold

```bash
cargo build -p cordis-cli
target/debug/cordis-cli create my-app
cd my-app
```

The generated project is a cargo workspace: `app/` (the host, calling `cordis_cli::run`), `plugins/hello/` (an example plugin), and `cordis.yml` (the entry config).

### 2. Build and place the plugin

Plugins compile to dynamic libraries; copy the artifact into `plugins/`:

```bash
cargo build
cp target/debug/libcordis_hello.dylib plugins/   # macOS; use .so on Linux
```

### 3. Configure and run

`cordis.yml` is a list of entries:

```yaml
- id: 'hello'
  name: cordis-hello      # the `name` from the plugin's plugin_meta
  config:
    greeting: hi          # JSON config passed to the plugin
```

Run and stop:

```bash
../target/debug/cordis-cli                 # reads ./cordis.yml, scans ./plugins
# or: cordis-cli -c app.yml --plugins-dir ./plugins
# Ctrl-C (SIGINT/SIGTERM) shuts down gracefully
```

### 4. Writing a plugin

The `.so` plugin is the primary form. The `Cargo.toml` must be a cdylib and re-allow `unsafe_code`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
cordis-sdk = { path = "...", default-features = false }

[lints.rust]
unsafe_code = "allow"
```

`src/lib.rs` exports the agreed C ABI symbols (full protocol in [docs/abi.md](docs/abi.md)):

```rust
use cordis_sdk::{HostVtable, PLUGIN_API_VERSION, PluginHandle};

const META: &std::ffi::CStr =
    c"{\"name\":\"cordis-hello\",\"version\":\"0.1.0\",\"inject\":[],\"provide\":[]}";

#[unsafe(no_mangle)]
pub extern "C" fn plugin_api_version() -> u32 {
    PLUGIN_API_VERSION
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_create(host: *const HostVtable) -> *mut PluginHandle {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(host)).cast::<PluginHandle>()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispose(handle: *mut PluginHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut *const HostVtable) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn plugin_meta() -> *const std::ffi::c_char {
    META.as_ptr()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_apply(handle: *mut PluginHandle, _config: *const std::ffi::c_char) -> i32 {
    let vtable = unsafe { *(handle as *const *const HostVtable) };
    let message = c"hello from cordis-rs";
    unsafe { ((*vtable).log)(message.as_ptr()) };
    0
}
```

During apply, the plugin reaches Context capabilities through the SDK's `ContextBridge` (provide/get services, on/emit events, register disposers, spawn async tasks); every value crossing the boundary is a JSON string.

There is also an **in-process plugin** form (linked as a library, no ABI): build a `Plugin { name, inject, apply, .. }` and call `ctx.plugin(&plugin, config)`. This suits first-party plugins shipped with the app. Full example: [crates/cordis-sdk/examples/hello.rs](crates/cordis-sdk/examples/hello.rs).

## Usage differences from the JS original (for plugin authors)

If you know the JS version of cordis, these are the differences you will feel directly when writing plugins.

**Plugin forms**

- JS: three forms — a function plugin `export function apply(ctx, config)`, a class plugin (`static inject`), and an object plugin `{ apply }`.
- Rust: two forms — an in-process plugin built as `Plugin { name, inject, apply, .. }` (`apply: Rc<fn(&Context, &Rc<dyn Any>) -> Effect>`), or a `.so` plugin exporting C ABI symbols. There is no class form; config is `Rc<dyn Any>` in-process and JSON across the `.so` boundary.

**Dependency injection (inject)**

- JS: `static inject = ['timer']` or the `@Inject()` decorator.
- Rust: the `Plugin.inject` field, `ctx.inject(&["timer"], callback)`, or the `inject: [...]` entry field in the config file. The reactive semantics are unchanged: a fiber stays pending until its dependencies are ready, starts when they come online, and unloads when they go away.
- The `#[cordis::inject]` macro is currently a marker only; it does not transform code.

**Services**

- JS: `class Foo extends Service`, accessed through the `ctx.foo` proxy; accessing a service without inject throws.
- Rust: `#[service] struct Foo;` generates the `Service` impl and a typed accessor `ctx.foo()` (returning a handle that carries the caller's context); `ctx.get::<Foo>()` / `ctx.get_str("foo")` also work. An unavailable service yields `None` instead of throwing.
- The callable JS service `ctx.logger('name')` becomes `ctx.logger().named("name")`.

**Config validation and merging**

- JS: any Standard Schema V1 validator (the ecosystem commonly uses schemastery).
- Rust: a `.so` plugin exports `plugin_validate_config` (JSON in, `0` accepts); in-process plugins use `ctx.plugin_with_validator(..)`. Config merging moves from `Object.assign` semantics to the `Config` trait's `merge` method. Neither version supports async validation.

**Cleanup logic (effects)**

- JS: the plugin function returns a cleanup function / generator / Promise; `ctx.effect(fn)` registers additional cleanup.
- Rust: return an `Effect` enum (`Disposer` / `Async` / `Iterable` / …), built with the `sync_disposer()` / `async_disposer()` helpers; `ctx.effect(..)` is the same. Neither version has `ctx.onDispose` (removed upstream in v4).

**Events**

- The five dispatch modes keep their names and semantics: `emit` / `parallel` / `serial` / `bail` / `waterfall`; `on` / `once` are the same; listeners are removed automatically when their fiber unloads — there is no `off`.
- Arguments are `&[Rc<dyn Any>]` instead of rest parameters; `ctx.on` returns an `EffectHandle` instead of a `() => boolean` disposer; there is no TS `declare module 'cordis' { interface Events }` augmentation — event names are plain strings.
- On the `.so` side everything goes through `ContextBridge`, and event arguments are always serialized as a JSON array.
- Upstream v4 core has no `ready` / `dispose` lifecycle events; this port matches that.

**Timers**

- JS: `ctx.timeout(cb, ms)` / `ctx.interval` / `ctx.throttle` / `ctx.debounce` (mixed into ctx).
- Rust: associated functions such as `TimerService::timeout(&ctx, cb, ms)` with an explicit `ctx` argument; still bound to the fiber lifecycle — disposing cancels.

**Config files**

- Entry fields are fully aligned: `id` / `name` / `config` / `group` / `disabled` / `inject` / `isolate` / `intercept`; nested entry ids are still joined with `:`.
- The JS `!js` expression tag becomes `!expr` (a minijinja template with three built-in functions: `env()` / `platform()` / `base_url()`), and it may only appear inside `config`.
- The entry `name`: in JS it is an npm package name or module path; here it is the `name` from the `.so` plugin's `plugin_meta`. Built-in plugins keep the `@cordisjs/plugin-group` / `@cordisjs/plugin-include` / `@cordisjs/plugin-hmr` names so existing configs migrate cleanly.

**Hot module replacement (HMR)**

- JS: requires `node --expose-internals`; clears the module cache and re-imports, with source-level dependency analysis.
- Rust: no special runtime flag needed; dependency analysis uses the declarative `deps` metadata exported by each `.so`; config-file watching, the `hmr/change` event, and rollback-on-failure semantics are preserved.
- There is no built-in "rebuild on change" pipeline yet — rebuild and swap the artifact yourself (e.g. with `cargo watch`); on macOS, dynamic libraries with TLS are never unloaded, so reload artifacts must use content-hash names (`name@hash.so`).

## System design differences

These are the architectural points where the port deliberately diverges from the JS original. Details and rationale live in the module-level docs of each crate and in [docs/abi.md](docs/abi.md).

**Plugin loading: in-process modules ↔ cdylib + C ABI**

Upstream plugins are JS modules sharing the host process and heap, passing arbitrary objects freely. Here plugins compile to cdylibs; the host loads them via `libloading` and validates every exported symbol. A plugin only sees an opaque `PluginHandle` plus a `HostVtable` of function pointers; every value crossing the boundary is a JSON string, and **allocations never cross**. Before calling into a plugin, the host pushes a session binding the handle to the current fiber's Context, so one `.so` instance can serve many fibers without cross-talk. The cost: non-JSON object services cannot cross the boundary — a `.so` declares them in its `inject` metadata and the host resolves them.

**Concurrency: Node event loop ↔ tokio current-thread + LocalSet**

The port deliberately keeps the original's single-threaded semantics: `Rc` / `RefCell` throughout, no locks, and `Context` is `!Send`. The session registry is `thread_local`, so vtable calls from other threads fail silently instead of panicking. Plugins must not bring their own runtime; async work goes through the vtable `spawn` to be driven by the host.

**Context passing: `this` closures / Proxy ↔ explicit parameters + ShadowContext**

Upstream service methods reach the root context through `this` closures, and service access goes through the dynamic `ctx[name]` proxy. The port makes everything explicit: service methods receive a `&ShadowContext` (distinguishing the service's own scope from the caller's scope, `Deref`-ing to the caller), and the `#[service]` macro generates typed accessors and traced handles in place of the proxy; dynamic string-based channels such as `get_str` remain available.

**Memory and lifetimes: GC ↔ ownership**

The original relies on the GC for cycles; the port manages ownership by hand — the service store holds only weak references such as `Weak<Fiber>` to avoid `Rc` cycles, and the host keeps a liveness registry of plugin handles, checking it before invoking deferred callbacks (listeners / disposers) so freed plugin code is never called.

**Effects and error model**

The four JS effect return shapes are modeled as an `Effect` enum; internal errors use `Result`, degrading to C conventions across the ABI (`0` / null mean failure, with host-side logging).

**HMR: module-cache clearing ↔ atomic handle swap + rollback**

The original clears the Node module cache and re-imports; the port registers the new artifact, re-applies each affected entry, and rolls back to the old artifact on failure. The dependency graph comes from the `.so`'s declarative `deps` metadata instead of source analysis. macOS dyld never unloads images with TLS (tokio pulls in TLS), so reload artifacts are named by content hash to keep `dlopen` from returning the stale image.

**Crate layout mirrors the npm packages**

`cordis` is a facade crate (mirroring the npm `cordis` package); `cordis-loader` stands alone (mirroring `@cordisjs/plugin-loader`); `cordis-plugin-group` is just an alias crate for the loader's built-in group plugin — matching upstream, where that package is only a re-export.

## Repository layout

```
crates/
  cordis/                    facade crate: top-level re-exports
  cordis-core/               core runtime: context, fiber, events, registry, logger
  cordis-sdk/                plugin-author SDK: the only crate `.so` plugins need
  cordis-macros/             procedural macros: `#[service]`, `#[inject]`
  cordis-loader/             plugin loader: entry tree, group/include semantics, config loading
  cordis-plugin-group/       group plugin (nested entry trees)
  cordis-plugin-include/     include plugin (yaml/json file-backed subtrees)
  cordis-plugin-hmr/         HMR plugin (file watching, dependency classification, reload)
  cordis-plugin-timer/       timer plugin (timeout/interval/throttle/debounce)
  cordis-plugin-logger-console/  default console logger exporter
  cordis-utils/              shared utilities
  cordis-cli/                command-line launcher (cordis / cordis create)
fixtures/                    example `.so` plugins used by tests
docs/                        ABI protocol documentation (Chinese & English)
```

## Development

```bash
./scripts/quality.sh    # fmt + clippy + test + doc — required before committing
```

## References

- ABI protocol: [docs/abi.md](docs/abi.md) · [docs/abi_cn.md](docs/abi_cn.md)
- Upstream: [cordisjs/cordis](https://github.com/cordisjs/cordis) · paper [_A Programming Paradigm for Spatiotemporal Composability_](https://github.com/cordiverse/paper)

## License

MIT. This is an unofficial port and is not affiliated with the cordisjs organization.
