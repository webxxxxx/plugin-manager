# 核心层技术选型

跨平台服务管理与插件系统的核心层上下文：一个 svcd 守护进程、一个 .spk 插件格式、一套 OSAL。这里只收本上下文特有的词，不收通用编程概念。

## Language

**服务（Service）**:
受 svcd 监督、可被启停与调用的单元；有 id、kind（core/plugin）、状态、依赖与能力面。
_Avoid_: 进程（进程只是它的运行载体）、插件（插件是服务的一种）

**Core 服务（Core service）**:
不经 .spk、不走插件安装流程的服务；管理员用启动脚本或可执行文件在 `services.d/*.json` 声明，由 svcd 监督。信任来自 root 所有的定义文件与本机管理员。
_Avoid_: 内置插件、系统插件、core plugin

**插件（Plugin）**:
从 .spk 安装、`kind=plugin` 的服务；能力由 manifest 声明。
_Avoid_: PluginManager、PM（已退役）

**svcd**:
唯一的守护进程与进程监督者；也是 multicall 二进制的守护进程入口名。
_Avoid_: 服务管理器进程、supervisor daemon

**svc**:
与服务管理器交互的 CLI，也是核心服务的能力命名空间（`svc.list`、`svc.describe` 等）。
_Avoid_: svcd CLI、管理客户端

**plugin**:
只管理插件的 CLI；动词与 `svc` 同源，但作用域限定在 `kind=plugin`，另加 `install / uninstall / editconfig / export / import`。
_Avoid_: 插件管理器、PluginManager（那是已退役的进程）

**mcp**:
multicall 二进制的 MCP 桥入口名；把 MCP 客户端接到常驻 svcd。
_Avoid_: MCP server、stdio server

**multicall 二进制**:
一个可执行文件，按 argv[0]/子命令分派出 `svcd`、`svc`、`plugin`、`mcp` 四个入口。
_Avoid_: 多合一程序、fat binary

**能力（Capability）**:
服务拥有的一项可调用操作；在 MCP 工具面上以 `<serviceId>.<capability>` 暴露。
_Avoid_: 工具、方法、action

**能力面（Capability surface）**:
一个服务在 MCP 工具面上的可见能力集合；死的只有 start，活的加上 manifest 能力。
_Avoid_: 工具列表、API

**启动（Start）**:
让一个死服务进入 ready 的准入动作；死服务在可调用面上唯一暴露的能力。
_Avoid_: 拉起、boot、run

**注册表（Registry）**:
svcd 持有的唯一服务清单；`svc list` 与 MCP 工具面都从它投影。
_Avoid_: 服务表、清单、catalog
