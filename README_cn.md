# Cordis(Rust 版)

[English](README.md) · 中文(本文件)

[Cordis](https://github.com/cordisjs/cordis) 的**非官方 Rust 实现** —— 一个面向插件化应用的元框架(A Meta-Framework of Spatiotemporal Composability)。

> 本项目是从零开始的移植,与官方 Cordis 项目没有隶属或背书关系。
> 上游与本项目都处于活跃开发中,API 尚不稳定,可能随时变更。

## 版本对齐

本项目的 API 与系统设计跟随上游 **`cordis` 4.0.0-rc.8**(JS/TS 版,仓库 `packages/core`)。

为了让跟随关系一眼可查,本仓库所有 crate 共用一个 workspace 版本号,并**与上游核心包保持一致:当前版本 `4.0.0-rc.8`**。上游升级后,本仓库完成适配再把版本号 bump 到相同值。

各 crate 与上游包的对应关系:

| 上游包(npm) | 上游版本 | 本仓库 crate |
| --- | --- | --- |
| `cordis` | 4.0.0-rc.8 | `cordis` / `cordis-core` |
| `@cordisjs/plugin-loader` | 1.0.0-rc.5 | `cordis-loader` |
| `@cordisjs/plugin-group` | 1.0.0 | `cordis-plugin-group` |
| `@cordisjs/plugin-include` | 1.0.4 | `cordis-plugin-include` |
| `@cordisjs/plugin-hmr` | 1.0.15 | `cordis-plugin-hmr` |
| `@cordisjs/plugin-timer` | 1.1.2 | `cordis-plugin-timer` |
| `@cordisjs/plugin-logger-console` | 1.0.0 | `cordis-plugin-logger-console` |
| `@cordisjs/utils` | 1.0.0 | `cordis-utils` |
| `create-cordis` | 0.3.0 | `cordis-cli create`(本地模板) |

另有独立于 crate 版本号的**插件 ABI 版本**:`PLUGIN_API_VERSION = 3`。host 加载 `.so` 插件时按严格相等校验,不匹配即拒绝加载。详见 [docs/abi_cn.md](docs/abi_cn.md)([English](docs/abi.md))。

## 快速上手

> 本项目尚未发布到 crates.io,以下均以源码方式使用(需要 Rust 1.97+)。

一个 cordis 应用由两部分组成:**宿主程序**(app)和**插件**(编译为动态库的 cdylib)。运行时读取 `cordis.yml`,扫描 `plugins/` 目录加载其中的 `.so` / `.dylib`。

### 1. 脚手架

```bash
cargo build -p cordis-cli
target/debug/cordis-cli create my-app
cd my-app
```

生成的工程是一个 cargo workspace:`app/`(宿主,调用 `cordis_cli::run`)、`plugins/hello/`(示例插件)、`cordis.yml`(入口配置)。

### 2. 构建并放置插件

插件编译为动态库后,把产物放进 `plugins/` 目录:

```bash
cargo build
cp target/debug/libcordis_hello.dylib plugins/   # macOS;Linux 为 .so
```

### 3. 配置与启动

`cordis.yml` 是入口(entry)列表:

```yaml
- id: 'hello'
  name: cordis-hello      # 插件 plugin_meta 中的 name
  config:
    greeting: hi          # 传给插件的 JSON 配置
```

启动与退出:

```bash
../target/debug/cordis-cli                 # 默认读 ./cordis.yml、扫描 ./plugins
# 或: cordis-cli -c app.yml --plugins-dir ./plugins
# Ctrl-C(SIGINT/SIGTERM)优雅退出
```

### 4. 编写插件

`.so` 插件是主要形态。`Cargo.toml` 必须是 cdylib,并自行放开 `unsafe_code`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
cordis-sdk = { path = "...", default-features = false }

[lints.rust]
unsafe_code = "allow"
```

`src/lib.rs` 导出约定的 C ABI 符号(完整协议见 [docs/abi_cn.md](docs/abi_cn.md)):

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

插件在 apply 期间通过 SDK 的 `ContextBridge` 访问 Context 能力(注册服务、读写服务、监听/发送事件、注册 disposer、spawn 异步任务),跨边界的值一律是 JSON 字符串。

此外还有一种**进程内插件**形态(作为库直接链接,不走 ABI):构造 `Plugin { name, inject, apply, .. }` 再 `ctx.plugin(&plugin, config)`,适合应用自带的一等插件。完整示例见 [crates/cordis-sdk/examples/hello.rs](crates/cordis-sdk/examples/hello.rs)。

## 与 JS 原版的用法差异(面向插件作者)

如果你用过 JS 版 cordis,以下是写插件时会直接感受到的差异。

**插件形态**

- JS:三种形态 —— 函数插件 `export function apply(ctx, config)`、class 插件(`static inject`)、对象插件 `{ apply }`。
- Rust:两种形态 —— 进程内插件构造 `Plugin { name, inject, apply, .. }`(`apply: Rc<fn(&Context, &Rc<dyn Any>) -> Effect>`);`.so` 插件导出 C ABI 符号。没有 class 形态;配置在进程内是 `Rc<dyn Any>`,在 `.so` 侧是 JSON。

**依赖注入(inject)**

- JS:`static inject = ['timer']` 或 `@Inject()` 装饰器。
- Rust:`Plugin.inject` 字段、`ctx.inject(&["timer"], callback)`,或 entry 配置里的 `inject: [...]`。反应式语义不变:依赖未就绪时 fiber 处于 pending,服务上线自动启动、下线自动卸载。
- `#[cordis::inject]` 宏目前只是标记,不改变代码。

**服务(service)**

- JS:`class Foo extends Service`,经 proxy `ctx.foo` 访问;未 inject 直接访问抛错。
- Rust:`#[service] struct Foo;` 宏生成 `Service` impl 与类型化访问器 `ctx.foo()`(返回携带调用点上下文的 handle);也可 `ctx.get::<Foo>()` / `ctx.get_str("foo")`。服务未就绪返回 `Option`,不抛异常。
- JS 的可调用服务 `ctx.logger('name')` → Rust 的 `ctx.logger().named("name")`。

**配置校验与合并**

- JS:任意 Standard Schema V1 校验器(生态常用 schemastery)。
- Rust:`.so` 插件导出 `plugin_validate_config`(JSON 进,0 通过);进程内插件用 `ctx.plugin_with_validator(..)`。配置合并从 `Object.assign` 语义变为 `Config` trait 的 `merge` 方法。两个版本都不支持异步校验。

**清理逻辑(effect)**

- JS:插件函数返回清理函数 / generator / Promise;`ctx.effect(fn)` 注册额外清理。
- Rust:返回 `Effect` 枚举(`Disposer` / `Async` / `Iterable` / …),用 `sync_disposer()` / `async_disposer()` 辅助构造;`ctx.effect(..)` 相同。

**事件**

- 五种派发模式同名同义:`emit` / `parallel` / `serial` / `bail` / `waterfall`;`on` / `once` 相同;监听器随 fiber 卸载自动移除,没有 `off`。
- 参数是 `&[Rc<dyn Any>]` 而非 rest 参数;`ctx.on` 返回 `EffectHandle` 而非 `() => boolean`;没有 TS 的 `declare module 'cordis' { interface Events }` 类型增强,事件名就是普通字符串。
- `.so` 插件侧走 `ContextBridge`,事件参数一律序列化为 JSON 数组。
- 上游 v4 core 没有 `ready` / `dispose` 生命周期事件,本移植保持一致。

**定时器**

- JS:`ctx.timeout(cb, ms)` / `ctx.interval` / `ctx.throttle` / `ctx.debounce`(mixin 到 ctx)。
- Rust:`TimerService::timeout(&ctx, cb, ms)` 等关联函数,显式传 `ctx`;同样绑定 fiber 生命周期,dispose 即取消。

**配置文件**

- entry 字段完全对齐:`id` / `name` / `config` / `group` / `disabled` / `inject` / `isolate` / `intercept`;嵌套 entry 的 id 同样用 `:` 分层。
- JS 的 `!js` 表达式标签 → Rust 的 `!expr`(minijinja 模板,内置 `env()` / `platform()` / `base_url()` 三个函数),且只允许出现在 `config` 字段。
- entry 的 `name`:JS 是 npm 包名或模块路径;Rust 是 `.so` 插件 `plugin_meta` 里的 name。内建插件沿用 `@cordisjs/plugin-group` / `@cordisjs/plugin-include` / `@cordisjs/plugin-hmr` 名称,便于迁移既有配置。

**热重载(HMR)**

- JS:需要 `node --expose-internals`,清理模块缓存后重新 import,做源码级依赖分析。
- Rust:不需要特殊 runtime flag;依赖分析改用 `.so` 导出的声明式 `deps` 元数据;配置文件监听、`hmr/change` 事件、失败回滚语义一致。
- 目前没有内建"改动后自动重新编译"流程,需自行用 `cargo watch` 等重建并替换产物;macOS 上含 TLS 的动态库永不卸载,重载产物需用内容哈希命名(`name@hash.so`)。

## 系统设计差异

以下是移植在架构层面与 JS 原版的核心分歧,细节与决策动机见各 crate 的模块级文档与 [docs/abi_cn.md](docs/abi_cn.md)。

**插件加载:同进程模块 ↔ cdylib + C ABI**

原版插件就是 JS 模块,与宿主同进程同堆,任意对象自由互传。移植版插件编译为 cdylib,host 经 `libloading` 加载并逐一校验导出符号;插件对宿主只见不透明 `PluginHandle` 与 `HostVtable` 函数指针表,跨边界的值一律是 JSON 字符串,**分配永不跨界**。host 调用插件前压入"handle ↔ 当前 fiber Context"会话,同一个 `.so` 实例可被多个 fiber 共享而不串味。代价:非 JSON 的对象服务无法跨边界,`.so` 侧只能通过元数据 `inject` 声明依赖、由 host 解析。

**并发模型:Node 事件循环 ↔ tokio current-thread + LocalSet**

刻意保留原版的单线程语义:全链路 `Rc` / `RefCell`、无锁,`Context` 是 `!Send`。会话注册表是 `thread_local` 的,跨线程调用 vtable 会静默失败而非 panic。插件禁止自带 runtime,异步只能经 vtable `spawn` 交给 host 驱动。

**上下文传递:`this` 闭包 / Proxy ↔ 显式参数 + ShadowContext**

原版服务方法经 `this` 闭包拿到 root context,服务访问走 `ctx[name]` 动态 proxy。移植版全部改为显式传参:服务方法接收 `&ShadowContext`(内部区分"服务自身作用域"与"调用方作用域",`Deref` 到调用方),`#[service]` 宏生成类型化访问器与 traced handle 替代 proxy;同时保留 `get_str` 等动态字符串通道。

**内存与生命周期:GC ↔ 所有权**

原版依赖 GC 处理环;移植版用所有权手工管理——服务 store 只持有 `Weak<Fiber>` 等弱引用以避免 `Rc` 循环,host 维护存活句柄注册表,事件监听 / disposer 这类延迟回调在调用插件前检查句柄存活,绝不调用已释放的插件代码。

**Effect 与错误模型**

JS 的四种 effect 返回值形态建模为 `Effect` 枚举;内部错误用 `Result`,跨 ABI 边界退化为 C 约定(0 / null 表示失败,host 侧记日志)。

**HMR:清模块缓存 ↔ 句柄原子交换 + 回滚**

原版清空 Node 模块缓存后重新 import;移植版改为注册新产物、逐个重 apply 受影响 entry、失败整体回滚旧产物。依赖图来自 `.so` 的声明式 `deps` 元数据而非源码分析。macOS dyld 永不卸载含 TLS 的镜像(tokio 引入 TLS),因此重载产物按内容哈希命名,保证 `dlopen` 不拿到旧镜像。

**crate 划分对齐 npm 包**

`cordis` 是 facade crate(镜像 npm `cordis` 包);`cordis-loader` 独立(镜像 `@cordisjs/plugin-loader`);`cordis-plugin-group` 只是 loader 内建 group 插件的别名 crate,与上游该包仅是 re-export 的做法一致。

## 仓库结构

```
crates/
  cordis/                    facade crate:顶层 re-export
  cordis-core/               核心运行时:context、fiber、事件、registry、logger
  cordis-sdk/                插件作者 SDK:`.so` 插件只依赖它
  cordis-macros/             过程宏:`#[service]`、`#[inject]`
  cordis-loader/             插件加载器:entry 树、group/include 语义、配置加载
  cordis-plugin-group/       group 插件(嵌套 entry 树)
  cordis-plugin-include/     include 插件(yaml/json 文件挂载子树)
  cordis-plugin-hmr/         HMR 插件(文件监听、依赖分类、重载)
  cordis-plugin-timer/       timer 插件(timeout/interval/throttle/debounce)
  cordis-plugin-logger-console/  默认 console 日志输出
  cordis-utils/              共享工具
  cordis-cli/                命令行启动器(cordis / cordis create)
fixtures/                    测试用 `.so` 插件样例
docs/                        ABI 协议文档(中英双语)
```

## 开发

```bash
./scripts/quality.sh    # fmt + clippy + test + doc,提交前必跑
```

## 参考

- ABI 协议:[docs/abi_cn.md](docs/abi_cn.md) · [docs/abi.md](docs/abi.md)
- 上游:[cordisjs/cordis](https://github.com/cordisjs/cordis) · 论文 [_A Programming Paradigm for Spatiotemporal Composability_](https://github.com/cordiverse/paper)

## License

MIT。本项目为非官方移植,与 cordisjs 组织无关。
