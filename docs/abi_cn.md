# `.so` 插件 ABI 协议

本文档是 host(`cordis-loader`)与 `.so` 插件(`cordis-sdk`)之间的跨库协议:
入口符号、版本号、vtable 布局、句柄生命周期、分配器纪律与错误约定。

> English version: [docs/abi.md](abi.md) · 中文版:本文件。

## 1. 入口符号与版本号

每个插件 cdylib 必须导出以下符号;host 在加载时逐一校验,缺失或版本不匹配即拒绝加载:

| 符号 | 签名 | 说明 |
| --- | --- | --- |
| `plugin_api_version` | `fn() -> u32` | 插件实现的 ABI 版本,必须等于 host 的 `PLUGIN_API_VERSION` |
| `plugin_create` | `fn(*const HostVtable) -> *mut PluginHandle` | 创建插件实例;版本不匹配返回 null |
| `plugin_dispose` | `fn(*mut PluginHandle)` | 销毁插件实例 |
| `plugin_meta` | `fn() -> *const c_char` | (可选)元数据 JSON:`name`/`version`/`inject`/`provide`/`deps` |
| `plugin_validate_config` | `fn(*const c_char) -> i32` | (可选)配置校验,0 通过、非 0 拒绝 |
| `plugin_apply` | `fn(*mut PluginHandle, *const c_char) -> i32` | (可选)应用配置;在 host 会话内执行 |

当前版本:`PLUGIN_API_VERSION = 3`。

- v2:入口协议 + async 桥(`log`/`spawn`)。
- v3:vtable 新增 `provide`/`get`/`on`/`emit`/`effect_disposer` 五个 Context 桥入口。

`deps` 声明插件链接的 host crate/服务,HMR 插件用它做依赖分类。

## 2. Host vtable 布局

`HostVtable` 是 `#[repr(C)]` 结构,host 在 `plugin_create` 时传入,插件在整个生命周期内持有其指针。
所有函数指针都只在 host 线程上被调用(单线程纪律,见 §6)。

```rust
pub struct HostVtable {
    pub log: extern "C" fn(message: *const c_char),          // 日志
    pub spawn: HostSpawn,                                    // 异步桥
    pub provide: HostProvide,                                // 注册服务
    pub get: HostGet,                                        // 读取服务
    pub on: HostOn,                                          // 注册事件监听
    pub emit: HostEmit,                                      // 发送事件
    pub effect_disposer: HostEffectDisposer,                 // 注册 fiber disposer
    pub data: *mut c_void,                                   // host runtime 句柄
    pub host_version: u32,                                   // host ABI 版本
}
```

### Context 桥入口语义

| vtable 入口 | 签名 | 语义 |
| --- | --- | --- |
| `provide` | `fn(handle, name, payload_json) -> i32` | 在插件当前 fiber 上注册服务;`payload_json` 为 JSON 值;0 成功、非 0 失败(重复注册/无会话) |
| `get` | `fn(handle, name) -> *const c_char` | 读回服务,序列化为 JSON 字符串;缺失或不可序列化返回 null |
| `on` | `fn(handle, event, callback) -> *mut c_void` | 注册监听;返回 host 持有的不透明 listener 句柄;随 fiber 卸载自动移除 |
| `emit` | `fn(handle, event, payload_json)` | 发送事件;`payload_json` 为 JSON 数组(事件参数) |
| `effect_disposer` | `fn(handle, disposer_fn)` | 在插件当前 fiber 上注册 disposer;卸载时按注册逆序执行 |

插件侧统一通过 SDK 的 `ContextBridge` 调用这些入口:

```rust
use cordis_sdk::{ContextBridge, HostVtable, PluginHandle};

unsafe extern "C" fn plugin_apply(handle: *mut PluginHandle, config: *const c_char) -> i32 {
    let vtable = /* plugin_create 时保存的 host vtable */;
    // SAFETY: 当前调用处于 host 会话内。
    let bridge = unsafe { ContextBridge::new(vtable, handle) };
    bridge.provide("greeting", "\"hello\"").unwrap();
    let _ = bridge.get("greeting");                    // Some("\"hello\"")
    bridge.on("demo/event", on_demo_event).unwrap();
    bridge.effect_disposer(disposer);
    bridge.emit("demo/event", "[\"hi\"]");
    0
}
```

## 3. 会话模型(handle → Context)

`plugin_apply` 以及 host 回调插件的事件监听/disposer 时,host 在调用前压入一个**会话**:
把插件 `handle` 与当前 fiber 的 `Context` 绑定。vtable 入口按传入的 `handle`
找到最内层匹配会话,再在对应的 `Context` 上执行 `provide`/`get`/`on`/`emit`/
`effect_disposer`。

要点:

- 同一个 `.so` 实例可以被多个 fiber 共享(多实例 fixture);会话按"handle + 调用时刻"
  区分,因此一个 fiber 注册的服务不会泄漏进另一个 fiber。
- 会话可以嵌套:插件事件回调里再调 `emit` 或注册 disposer 时,压入新会话,返回时弹出。
- 会话只在 host 线程存在;从其他线程调用 vtable 入口会静默失败(`provide` 返回非 0、
  `get` 返回 null、`on` 返回 null)。
- **异步限制**:`spawn` 的任务由 host runtime 驱动,但任务体运行在会话之外;
  插件不应在 spawn 出的异步代码里调用 `provide`/`get`/`on`/`emit`/`effect_disposer`。
  需要跨异步使用数据时,应在 apply/回调里先 `get` 拷贝出来,或注册好监听。

## 4. 句柄生命周期

- `plugin_create` 成功返回后,handle 由 host 的 `SoPlugin` 持有;`SoPlugin::drop`
  调用 `plugin_dispose` 并注销 handle。
- host 维护一个存活句柄注册表;事件监听与 disposer 这类**延迟回调**在调用插件前
  检查 handle 是否仍存活。若插件实例已被 dispose 而 fiber 尚未卸载,回调会被跳过
  (记一条错误日志),绝不调用已释放的插件代码。
- `on` 返回的 listener 句柄是 host 持有的不透明指针,仅用于标识,插件不应解引用;
  本版未提供 `off`,监听随 fiber 卸载自动移除。

## 5. 分配器纪律与值传递

**分配永不跨界**。

- 跨边界的值一律以 JSON 字符串承载;调用方在调用期间持有字符串,host 立即拷贝解析。
- `get` 的返回值指向 host 会话内的临时缓冲区,只在**下一次 host 调用进入同一会话之前**
  有效;插件必须立即拷贝(SDK 的 `ContextBridge::get` 已拷贝)。
- 事件参数由 host 序列化为 JSON 数组;不可序列化的参数(`Rc<dyn Any>` 对象)编码为 `null`。
- 非 JSON 对象服务(如 host 侧 Rust 服务)无法跨边界,`get` 返回 null;
  插件应通过元数据 `inject` 声明依赖,由 host 侧完成解析。

## 6. 线程模型

host 任务为 `Send`,运行在 tokio worker 池上(多线程化计划阶段 2):

- 编译期:host 与 SDK 状态均为 `Send + Sync`(`Arc`、原子类型、无锁快照和短临界区
  `Mutex`)。
- **插件 Send 契约(阶段 3 决策)**:经 `spawn` 交给 host 的插件 future 必须是
  `Send` 且与线程无关。host 可能在任意 runtime 线程上 poll 插件 future,并在
  await 点之间把它迁移到其他线程;插件代码不得依赖 thread-local 状态或某个固定
  host 线程。
- 运行期:会话在当前驱动 host→plugin 调用的线程上压栈;没有会话的线程调用 vtable
  会静默失败而不会 panic。存活句柄注册表是进程级的,任何线程上的延迟回调都会跳过
  已 dispose 的插件。
- 插件禁止自带 runtime;异步只能经 SDK 的 `spawn(vtable, future)` 交给 host。

## 7. 错误约定

- `plugin_create`:版本不匹配/无效 vtable 返回 null。
- `plugin_validate_config` / `plugin_apply`:0 成功、非 0 失败。
- `provide` / `on`:0/null 之外的返回值表示失败(重复注册、无会话、fiber 已卸载)。
- `get` / `emit`:失败时返回 null / 静默忽略;host 侧通过 logger 记录诊断信息。

## 8. SDK 公开面清单(`cordis::Context` 在 `.so` 内的形态)

`.so` 插件无法直接持有 core 的 `Context`,等价物是 vtable + `ContextBridge`:

| core API | `.so` 内形态 | 说明 |
| --- | --- | --- |
| `ctx.provide(name, value)` | `bridge.provide(name, json)` | 值必须是 JSON 可序列化数据 |
| `ctx.get(name)` | `bridge.get(name) -> Option<String>` | 只读回 JSON 数据服务 |
| `ctx.on(event, cb)` | `bridge.on(event, callback)` | 回调签名 `fn(handle, args_json)` |
| `ctx.emit(event, ...args)` | `bridge.emit(event, args_json)` | 参数为 JSON 数组 |
| `Effect::Disposer(d)` | `bridge.effect_disposer(fn)` | 随 fiber 卸载按逆序执行 |
| `ctx.logger()` | vtable `log` | 仅字符串日志,无结构化 logger |
| `tokio::task::spawn_local` | SDK `spawn(vtable, future)` | 异步任务交给 host runtime |

限制:

- 无法跨边界传递/调用对象服务;只支持数据服务与事件。
- apply 阶段 fiber 处于 `LOADING`,严格服务查找(`get`)要求 provider fiber 为
  `ACTIVE`,因此 apply 内读自己刚 provide 的值会失败;应在事件回调等 fiber
  `ACTIVE` 的时刻读取(见 Context 桥 fixture)。
- 监听/disposer 的生命周期绑定 fiber,不能手动解除(`off` 后续版本提供)。
