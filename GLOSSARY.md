# 核心层技术选型

跨平台服务管理与插件系统的核心层上下文：一个 svcd 守护进程、一个 .spk 插件格式、一套 OSAL。这里只收本上下文特有的词，不收通用编程概念。

## Language

**服务（Service）**:
受 svcd 监督、可被启停与调用的单元；有 id、kind（core/plugin）、状态、依赖与能力面。
_Avoid_: 进程（进程只是它的运行载体）、插件（插件是服务的一种）

**受管服务（Managed service）**:
受 svcd 监督、由它启停与重启的 core 或 plugin；svcd 自身（内置 `svc`）不在其列。
_Avoid_: 内置服务、系统服务

**Core 服务（Core service）**:
不经 .spk、不走插件安装流程的服务；管理员用启动脚本或可执行文件在 `services.d/*.json` 声明，由 svcd 监督。信任来自 root 所有的定义文件与本机管理员。
_Avoid_: 内置插件、系统插件、core plugin

**插件（Plugin）**:
从 .spk 安装、`kind=plugin` 的服务；能力由 manifest 声明，schema 2 起也可声明生命周期动作。
_Avoid_: PluginManager、PM（已退役）

**脚本插件（Script plugin）**:
payload 是脚本、由 manifest 的 `interpreter` 声明宿主解释器、并可用 `lifecycle` 声明 start/stop/status 的插件；执行路径与 core 脚本共用，来源与信任仍是 .spk。
_Avoid_: core plugin、系统插件

**解释器（Interpreter）**:
脚本插件依赖的宿主程序；manifest 只写 PATH 上的裸名、可选参数与可选版本约束。安装时解析为绝对路径并校验版本；守护进程重启扫描时只重新解析路径、不重复探测版本，运行时用解析出的绝对路径。
_Avoid_: runtime、引擎

**svcd**:
唯一的守护进程与进程监督者；也是 multicall 二进制的守护进程入口名。
_Avoid_: 服务管理器进程、supervisor daemon

**svc**:
与服务管理器交互的 CLI，也是核心服务的能力命名空间（`svc.list`、`svc.describe` 等）。
_Avoid_: svcd CLI、管理客户端

**plugin**:
只管理插件的 CLI；动词与 `svc` 同源，但作用域限定在 `kind=plugin`，另加 `install / uninstall / editconfig / export / import`。
_Avoid_: 插件管理器、PluginManager（那是已退役的进程）

**svcd.conf**:
`<config>` 下的运行时配置文件，由 `svcd --install --conf <file>` 写入；承载 HTTP 端口、LAN 默认开关、plugin 安装位置与 core 服务扫描位置。
_Avoid_: settings.json、通用“配置文件”

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

**Era（协议代）**:
MCP 客户端与服务器使用的协议形态：modern 用 `2026-07-28` 的每请求 `_meta`，legacy 用 `initialize` 握手（`2025-11-25` 及以前），dual-era 两者都支持。
_Avoid_: 版本（具体版本号是日期；Era 是协议形态）

**Streamable HTTP**:
MCP 的 HTTP 传输：单个 `POST /mcp` 端点，每个请求独立、无 session；v1 只返回 JSON，不开 SSE。
_Avoid_: HTTP MCP server、SSE 传输（v1 没有 SSE）

**启动（Start）**:
让一个死服务进入 ready 的准入动作；死服务在可调用面上唯一暴露的能力。
_Avoid_: 拉起、boot、run

**注册表（Registry）**:
svcd 持有的唯一服务清单；`svc list` 与 MCP 工具面都从它投影。
_Avoid_: 服务表、清单、catalog

**状态机核（Machine）**:
6 态纯核；只接收带 pid 的事件、产出动作，不碰进程、时钟或 IO。由监督槽执行动作。
_Avoid_: supervisor、状态机（不带「核」）

**监督槽（Slot）**:
一个被监督服务的运行时载体：状态机核 + 子进程 + stdio 管线 + 定时器。
_Avoid_: 实例、worker
