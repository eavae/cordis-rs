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

插件是遵循插件 ABI 的 cdylib。协议(导出符号、元数据格式)与完整示例见
[docs/abi_cn.md](docs/abi_cn.md);`cordis-sdk` crate 的文档覆盖编写 API。
此外还有**进程内插件**形态(作为库直接链接,不走 ABI),适合应用自带的一等
插件,示例见 [crates/cordis-sdk/examples/hello.rs](crates/cordis-sdk/examples/hello.rs)。

## 系统设计差异

以下是移植在架构层面与 JS 原版的核心分歧,细节与决策动机见各 crate 的模块级文档与 [docs/abi_cn.md](docs/abi_cn.md)。

**插件加载:同进程模块 ↔ cdylib + C ABI**

原版插件就是 JS 模块,与宿主同进程同堆,任意对象自由互传。移植版插件编译为 cdylib,host 经 `libloading` 加载并逐一校验导出符号;插件对宿主只见不透明句柄与宿主函数指针表,跨边界的值一律是 JSON 字符串,**分配永不跨界**。host 调用插件前压入"handle ↔ 当前 fiber Context"会话,同一个 `.so` 实例可被多个 fiber 共享而不串味。代价:非 JSON 的对象服务无法跨边界,`.so` 侧只能通过元数据 `inject` 声明依赖、由 host 解析。

**并发模型:Node 事件循环 ↔ 多线程 tokio runtime**

上游一切都在单事件循环上驱动。本移植跑在多线程 tokio runtime 上:核心
数据结构 `Send + Sync`(`Arc` + `parking_lot`),生命周期与异步任务经
`tokio::spawn` 分发到 worker 线程执行。loader 的 `.so` 会话注册表是
`thread_local` 的,跨线程调用 vtable 会静默失败而非 panic。插件禁止自带
runtime,异步只能经 host 的 spawn 交给宿主驱动。

**上下文传递:`this` 闭包 / Proxy ↔ 显式参数**

原版服务方法经 `this` 闭包拿到 root context,服务访问走 `ctx[name]` 动态 proxy。移植版全部改为显式传参:服务方法接收显式的 shadow context(区分"服务自身作用域"与"调用方作用域"),以类型化访问器替代 proxy;同时保留动态字符串访问通道。

**内存与生命周期:GC ↔ 所有权**

原版依赖 GC 处理环;移植版用所有权手工管理——服务 store 只持有弱引用以避免引用循环,host 维护存活句柄注册表,事件监听 / disposer 这类延迟回调在调用插件前检查句柄存活,绝不调用已释放的插件代码。

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
