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

A plugin is a cdylib that speaks the plugin ABI. The protocol (exported
symbols, metadata format) and a complete example live in
[docs/abi.md](docs/abi.md); the `cordis-sdk` crate docs cover the authoring
API. There is also an **in-process plugin** form (linked as a library, no
ABI) for first-party plugins shipped with the app — see
[crates/cordis-sdk/examples/hello.rs](crates/cordis-sdk/examples/hello.rs).

## System design differences

These are the architectural points where the port deliberately diverges from the JS original. Details and rationale live in the module-level docs of each crate and in [docs/abi.md](docs/abi.md).

**Plugin loading: in-process modules ↔ cdylib + C ABI**

Upstream plugins are JS modules sharing the host process and heap, passing arbitrary objects freely. Here plugins compile to cdylibs; the host loads them via `libloading` and validates every exported symbol. A plugin only sees an opaque handle plus a host function-pointer table; every value crossing the boundary is a JSON string, and **allocations never cross**. Before calling into a plugin, the host binds a session from the handle to the current fiber's Context, so one `.so` instance can serve many fibers without cross-talk. The cost: non-JSON object services cannot cross the boundary — a `.so` declares them in its `inject` metadata and the host resolves them.

**Concurrency: Node event loop ↔ multi-threaded tokio runtime**

Upstream drives everything on a single event loop. This port runs on a
multi-threaded tokio runtime: core data structures are `Send + Sync`
(`Arc` + `parking_lot`), and lifecycle/async work is dispatched with
`tokio::spawn` so it can run on worker threads. The loader's `.so` session
registry is `thread_local`, so vtable calls from other threads fail silently
instead of panicking. Plugins must not bring their own runtime; async work
goes through the host's spawn so it is driven by the host.

**Context passing: `this` closures / Proxy ↔ explicit parameters**

Upstream service methods reach the root context through `this` closures, and service access goes through the dynamic `ctx[name]` proxy. The port makes everything explicit: service methods receive a shadow context that distinguishes the service's own scope from the caller's scope, and typed accessors replace the proxy; dynamic string-based access remains available.

**Memory and lifetimes: GC ↔ ownership**

The original relies on the GC for cycles; the port manages ownership by hand — the service store holds only weak references to avoid reference cycles, and the host keeps a liveness registry of plugin handles, checking it before invoking deferred callbacks (listeners / disposers) so freed plugin code is never called.

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
