# 插件规模与通信机制评审：20 / 80 / 不限

> 日期：2026-11-14
> 评审对象：本仓库 `src/` 当前实现（`main` @ `95736d2`），以及 `docs/specs/2026-11-12-核心层需求最终版.md`、`docs/research/2026-11-13-core-compat-and-performance.md` 的既有结论。
> 方法：代码走读 + 既有性能基线 + 本次补充的“工具目录规模”实验。
> 性质：这是缺陷与方案评审，不是已经拍板的规格。涉及协议、权限和进程模型的建议需要转成 ADR / issue 后再落地。

## 0. 结论先行

1. **协议选型不是主要问题。** JSON-RPC 2.0 + NDJSON 跑 stdio/UDS/命名管道，对嵌入式 20 个插件是合适的；真正的问题在**连接/线程模型、同步控制面、全局工具目录**。
2. **20 个插件：当前实现基本可用**，但需要控制工具 schema 体积、线程数和 boot 扫描时间；嵌入式应继续用 `--no-default-features` + musl。
3. **80 个插件：进程和内存还能承受，工具目录会先撞墙。** 本次实测用“每个插件 20 个能力 × 20 字段 schema”的合成目录，**10 个插件时 `tools/list` 为 614,406 B，20 个插件时 MCP stdio 桥直接报 `frame exceeds the 1 MiB limit`，80 个插件同样失败**。这不是理论值，是当前硬限制（`ipc::MAX_FRAME_BYTES = 1 MiB`）。
4. **“按需求启用”目前不成立。** 现在只有“安装 / `enabled` 开机自启 / 手动 `start` / 活着才露出能力”四件事；没有 lazy activation、没有空闲回收、没有按用户/租户的启用授权，也没有工具目录的过滤视图。80 个已安装但闲置的插件仍会在 `tools/list` 里各占一个 `<id>.start`。
5. **“服务器环境数量不限”不能靠一个 daemon 解决。** 当前是单进程、内存 `BTreeMap` 注册表、单机工具面、无远端 agent/网关。要横向扩展，必须把系统拆成 **无状态网关 + 有界 cell/agent + 插件进程** 三层；`svcd` 保留为 cell/agent，而不是继续膨胀成“万能单进程”。
6. **建议的优先级：** 先做 P0（目录分页/缓存/体积预算、控制面非阻塞、持久复用管理连接、全局准入与取消、插件 UI 按插件隔离），让 80 个插件和工具门户可用；再做 P1（lazy activation、空闲回收、按用户授权、资源隔离）；最后做 P2（网关/多机/目录索引、MCP 联邦）。

## 1. 现状：一个进程、四张面、四条通信链路

### 1.1 组件图

```text
MCP 客户端 / 工具门户 / svc / plugin CLI / 浏览器
        │ stdio / HTTP / UDS / Windows named pipe
        ▼
┌─────────────────────────────────────────────────────────────┐
│ svcd（单进程，也是所有插件进程的父进程）                      │
│                                                             │
│  Daemon 事件循环（单线程）                                    │
│    ├─ Registry：BTreeMap<id, Service>（安装即入表）           │
│    ├─ Slots：BTreeMap<id, Slot>（状态机 + 子进程 + stdio）     │
│    ├─ channel server：每连接一线程，每连接只处理一个请求       │
│    ├─ HTTP server：每连接一线程（管理 API / UI / /mcp）        │
│    └─ MCP 聚合：每次请求 new 一条管理连接                     │
│                                                             │
│  每个 notify 插件：stdin + stdout + stderr 两条 reader 线程    │
└─────────────────────────────────────────────────────────────┘
        │ stdio（NDJSON JSON-RPC）
        ▼
   plugin 进程 × N（各自独立进程；当前继承 svcd 的权限）
```

### 1.2 四条通信链路

| 链路 | 传输 | 分帧/协议 | 认证 | 当前用途 | 主要限制 |
|---|---|---|---|---|---|
| 本地管理 | Unix UDS 或 Windows 命名管道 | NDJSON JSON-RPC，一行一帧 | 帧内 `auth` = `svcd.token` | `svc` / `plugin` CLI、HTTP API、MCP 桥、UI `/rpc` 最终都走它 | 每次请求新建连接；`channel::serve` 一连接只读一行（`src/channel.rs:110-132`）；token 每次从磁盘读（`src/channel.rs:139-163`） |
| 插件控制 | 子进程 stdin/stdout | NDJSON JSON-RPC | 父子进程信任，无独立凭证 | `svcd.ping` 就绪、`tools/call`、`svcd.shutdown`、插件通知 | 1 MiB/帧；每插件 64 在途；30 s 固定超时（`src/ipc.rs:18`、`src/slot.rs:22-24`） |
| 插件 Web UI | HTTP 反代 `/plugin-ui/{id}/` | 静态文件 + `/rpc`（JSON-RPC）+ SSE/WS | 全局会话 cookie / Bearer | 插件页面、页面调用宿主、宿主推送通知 | 所有插件共用一个 origin 和 `Path=/` 会话 cookie；`/rpc` 信任 URL 里的插件 id（`src/api.rs:698-732`） |
| MCP | stdio **或** `POST /mcp` | stdio 双 era（legacy `initialize` / modern `2026-07-28`）；`POST /mcp` **仅 modern** | `/mcp` 用单一 Bearer 密码 | 把插件能力投影成 MCP 工具 | 全量工具列表、无分页/过滤；桥每次请求 new 一条管理连接；stdio 输出受 1 MiB 限制（`src/mcp.rs:278-302`、`:470-472`；`src/mcp_http.rs:1-6`） |

### 1.3 生命周期与“工具面”的现状

- 安装：`.spk` 验签、解包、写 `<data>/plugins/<id>/<version>`，然后进入同一张 `Registry`（`src/daemon.rs:96-111`、`src/plugin.rs:450-499`）。
- 启用：`enabled` 只是“开机是否自动 start”的标记文件（`<plugin_dir>/enabled`，`src/plugin.rs:485`）。它不是“用户是否被授权”，也不是“现在是否运行”。
- 启动：手动 `svc start` / `plugin start`，或 boot 时 `start_enabled`；启动动作在 Daemon 事件循环里同步等待（`src/daemon.rs:729-770`）。
- 工具面：`mcp::tools` 只看 `Registry` + `is_live`。**每个已安装插件都会有一个 `<id>.start`**；只有 `Running` 才追加 manifest 能力（`src/mcp.rs:24-53`、`src/daemon.rs:531-540`）。
- 停止：`svc stop` 同步等待状态机走完（`src/daemon.rs:943-981`）。

### 1.4 现有设计里值得保留的部分

- **插件独立进程**：崩溃隔离、权限模型清晰、不需要把所有插件塞进一个地址空间。
- **纯状态机核**：`Machine` 只做状态与动作，I/O 在 `Slot`，测试覆盖好（`src/supervisor.rs`）。
- **NDJSON + stdio**：对嵌入式体积友好，已有实测（`docs/research/2026-11-12-plugin-ipc-and-reverse-proxy.md`）；MCP 规范也允许自定义传输复用这套分帧。
- **事件唤醒**：`Wake` condvar 已把空闲 CPU 从 8–18% 降到 0–0.63% 单核（`docs/research/2026-11-13-core-compat-and-performance.md`）。
- **签名、逐文件哈希、原子安装、keep-old/replace、按插件日志轮转**：这些是规模化的基础，不应为了性能删掉。
- **feature gate**：`http` 关闭后嵌入式二进制约 -12%，适合 20 个插件的目标。

## 2. 三个规模的基线

### 2.1 既有实测（`docs/research/2026-11-13-core-compat-and-performance.md`）

| 指标 | 0 插件 | 30 插件 | 备注 |
|---|---:|---:|---|
| daemon RSS（musl/Linux） | 0.99 MB | 1.88 MB | 每插件约 30 KB 的 daemon 状态 |
| 每插件 RSS（Linux `/bin/sh`） | — | 1.65 MB | 30 个共约 49 MB |
| 每插件 RSS（Windows cmd） | — | 7.0 MB | 30 个共约 210 MB |
| 全部安装+启动 | — | 约 1.0–1.1 s | 约 34–35 ms/插件 |
| daemon 空闲 CPU | 0% | 0–0.63% 单核 | 事件唤醒后 |

这些数字说明：**20 个插件在 Linux 嵌入式上主要是“进程数 + 线程数 + 目录大小”问题，不是 daemon 堆内存问题**；80 个 PC 插件在 Windows 上要注意每插件约 7 MB 的脚本进程成本。

### 2.2 本次补充的实验：工具目录先撞 1 MiB

方法：WSL2 Debian + 当前 `target/release/svcd`（glibc 构建），生成 20 个能力 × 20 字段 schema 的合成插件，安装并启动 10/20/80 个，用 legacy MCP stdio 桥执行 `initialize` + `tools/list`，记录 stdout 字节数与退出码。

| 插件数 | `tools/list` 结果 | 说明 |
|---:|---|---|
| 10 | 成功，stdout 614,406 B | 每插件约 61 KB 目录，21 个工具 |
| 20 | **失败**，stdout 160 B，stderr `frame exceeds the 1 MiB limit` | 目录约 1.2 MB，超过 `ipc::MAX_FRAME_BYTES` |
| 80 | **失败**，同样 1 MiB 限制 | 目录约 4.9 MB |
| 80（安装+启动总耗时） | 3.778 s | 80 个最简 shell 插件，含 bundle/install/start |
| 80（daemon 线程数） | 163 | 约 2×N + 3：每插件 stdout/stderr 两条 reader |

### 2.2.1 实现后复测：80 插件 4 线程

在 Unix reactor（插件 stdout/stderr 统一由一个 poll 线程读取）+ 持久管理连接池落地后，用同一台 WSL2 Debian、80 个最简 shell 插件（`ready=notify`、无能力）复测：

| 指标 | reactor 前（同机 glibc） | reactor 后 |
|---|---:|---:|
| daemon 线程数 @80 | 163（约 2N + 3） | **4** |
| daemon 虚拟内存 @80 | 约 4.06 GB | **约 267 MB** |
| daemon RSS @80 | 约 6.2 MB | **约 3.5 MB** |

线程数下降来自：每个插件不再有 stdout/stderr 两条 reader 线程，统一进一个 poll 线程。虚拟内存下降主要来自 glibc 的 per-thread arena 与线程栈减少。注意：

- 这是 glibc 构建；musl 的数字会更低，不能直接与嵌入式绝对值比较。
- HTTP 连接、SSE/WS 仍然是 thread-per-connection；本表只覆盖插件 stdio 与管理连接池，不覆盖 HTTP/WS 的线程上限。
- 持久管理连接池只把“每次请求新建连接”降为“每个客户端复用连接”，并发连接上限仍未做成 reactor 级固定。

### 2.2.2 全 reactor 规模回归（2026-11-14）

在 Unix 全 reactor（插件、管理 UDS、HTTP、SSE、WS）落地后，用 release 构建复测：

| 场景 | 结果 |
|---|---|
| 80 个插件、空闲 | daemon **8 线程** |
| 80 插件 + 500 条管理 UDS 连接 | 峰值 **8 线程** |
| 0 插件 + 1000 个并发 HTTP 请求 | 峰值 **8 线程** |
| 单个插件 UI + 64 条 WebSocket 连接（`MAX_SUBSCRIBERS` 上限） | 峰值 **8 线程** |

结论：Unix 上线程数不随插件数、管理连接数、HTTP 连接数或 WS 连接数增长；Windows 仍使用线程 fallback。

注意事项：

- 这是**合成压力夹具**，schema 是刻意放大的；真实插件可能更小，也可能更大（例如带完整 JSON Schema、枚举、长描述的工具）。
- glibc 构建的 RSS 会受 per-thread arena 影响；上表不用于推断 musl 嵌入式内存，只用于证明**目录大小与 1 MiB 硬限制**。
- 但结论是确定的：**按当前实现，PC 规模 80 个插件、每个插件二十来个中等 schema 能力时，标准 MCP stdio 客户端拿不到工具列表**。这不是“慢”，是协议层失败。

### 2.3 三个规模的具体判断

| 规模 | 现状判断 | 首要动作 |
|---|---|---|
| 嵌入式 ≤20 | **可用，但要克制**。Linux `/bin/sh` 插件约 1.65 MB/个，20 个约 33 MB，加 daemon 约 2 MB；线程约 2N + 固定池。带 Web UI/Python 插件会显著变重。 | 继续 `--no-default-features` + musl；目录页/总大小设硬上限；默认 lazy activation；不支持插件 UI 或只按需；避免 Python/Node 解释器插件。 |
| PC ≤80 | **进程/内存能承受，目录和控制面不能**。Windows cmd 插件约 7 MB/个，80 个约 560 MB；实测 163 线程；带中等 schema 的目录在 20 个插件时就超过 1 MiB。 | P0 全部：目录分页/预算、非阻塞控制面、持久连接/准入、插件 UI 隔离；80 插件回归门禁。 |
| 服务器不限 | **单 daemon 不可行**：内存注册表、单机工具面、无远端 agent/路由。即使单机堆到上千插件，进程/线程/内存/故障域也不可接受。 | P2：无状态网关 + 有界 cell/agent + 目录索引 + placement/路由；按 cell 扩容。 |

## 3. 缺陷清单

严重度：**P0** = 到 80 或工具门户就不可用/有安全缺口；**P1** = 能跑但运维/隔离/体验明显不足；**P2** = 服务器/长期演进才必须。

| ID | 严重度 | 缺陷 | 关键证据 | 在哪个规模爆炸 |
|---|---|---|---|---|
| D1 | P0 | `tools/list` 全量构造、无分页/过滤/缓存，超过 1 MiB 直接失败 | `src/mcp.rs:24-53`、`:278-302`、`:470-472`；`src/ipc.rs:18` | 20（大 schema）~80 |
| D2 | P0 | Daemon 事件循环被 install/start/stop/status probe 同步阻塞 | `src/daemon.rs:729-770`、`:943-981`、`:1096-1152`、`:794` | 80 起明显；不限时不可用 |
| D3 | P0 | 每次请求新建管理连接 + 每连接一线程；MCP 桥每个请求都新建连接并重读 token | `src/channel.rs:99-105`、`:110-132`、`:139-163`；`src/api.rs:246-249` | 80 高并发/p99；不限时线程爆炸 |
| D4 | P0 | 没有“按需启用”：enabled 只是 boot 标志；无 lazy start、空闲回收、预热池；每个安装插件都暴露 start 工具 | `src/plugin.rs:485`、`src/mcp.rs:30-35`、`src/daemon.rs:280-313` | 80 闲置目录；工具门户 |
| D5 | P0 | 只有单一管理员密码；插件 UI 共用 `Path=/` cookie，`/rpc` 信任 URL 插件 id，A 插件页面可调 B 插件 | `src/lan.rs:224-241`、`src/api.rs:698-732`、`src/webui.rs:33-74` | 多插件即存在；门户多用户时不可接受 |
| D6 | P1 | 只有每插件 64 在途上限，没有全局准入/队列/公平调度；超时只发取消通知，不保证插件停止；无熔断/健康检查 | `src/slot.rs:22`、`:142-184`、`:552-575` | 80×64 潜在在途；服务器 |
| D7 | P1 | 无 CPU/内存/线程/速率隔离；插件继承 svcd 权限（root/System）；hooks 同权限 | `src/package.rs`、`src/slot.rs:244-300`、`src/proc.rs:94-172`；规格 #19 | 80 噪声邻居；多租户 |
| D8 | P0（服务器） | 单进程/单机/内存注册表，无远端 agent、无目录索引、无联邦 | `src/registry.rs:181-204`、`src/daemon.rs:96-111` | 不限 |
| D9 | P1 | 能力名/数量/schema 无长度与体积上限；工具名未按 MCP 规则校验；缺少 title/tags/annotations/outputSchema | `src/package.rs:471-486`、`src/mcp.rs:43-47` | 80/门户 |
| D10 | P1 | keep-old 升级时旧进程还在跑，但 Registry 已换成新 manifest 的能力面；工具列表与实际进程可能不一致 | `src/daemon.rs:807-850` | 热升级/80 |
| D11 | P1 | 无指标、无审计、无 trace；只有每插件 1 MiB×3 的日志轮转 | `src/log.rs:6-7`、`src/daemon.rs` 全局 | 门户/合规 |
| D12 | P2 | 固定 30 s、1 MiB 参数/结果、无进度/流式/blob、无反向 RPC；插件结果原样透传，没有 outputSchema 校验；宿主生成的错误/状态只有 text | `src/slot.rs:24`、`src/ipc.rs:18`、`src/mcp.rs:57-71`、`:380-386` | 长任务/富结果/文件类工具 |
| D13 | P2 | boot 时全量扫描 manifest、全量建 Slot；`start_enabled` 按层等待；status probe 同步 | `src/plugin.rs:450-499`、`src/daemon.rs:280-313`、`:1096-1152` | 不限 |

### 3.1 D1：工具目录是第一个硬墙

当前 `mcp::tools` 每次被调用都：

1. 遍历所有 plugin；
2. 每个 plugin 先塞一个 `<id>.start`；
3. 活着的追加全部能力；
4. 按名字排序；
5. 由 daemon 序列化成一行 JSON，经管理通道交给 MCP 桥；
6. 桥再序列化到 stdout，受 `ipc::MAX_FRAME_BYTES` 限制。

`registry.version` 还会把完整工具列表序列化后哈希（`src/daemon.rs:1073-1082`）；legacy 客户端每秒轮询一次（`src/mcp.rs:117-147`、`:356-375`）。也就是说，**目录越大，每次 tools/list 和每个 legacy 客户端的每秒轮询都越贵**。

MCP 2026-07-28 的 `tools/list` 明确支持 opaque cursor 分页，且允许工具集合随请求授权变化（[Pagination](https://modelcontextprotocol.io/specification/2026-07-28/server/utilities/pagination.md)、[Tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools.md)）。当前实现完全没有用这两个能力。结果是：

- 标准客户端在 1 MiB 处失败，而不是自动翻页；
- 即使不失败，把所有工具塞进模型上下文也会造成选择质量下降和 token 成本；
- 工具门户无法做“只看我启用的/我授权的工具”的视图。

### 3.2 D2：Daemon 的同步控制面

`start_layer` 在事件循环里循环 `pump + sleep(20ms)`，直到所有服务 Running 或 30 s 超时；`stop_service` 同样阻塞到 Stopped/Failed；`plugin.install` 在事件循环里做包验签、解包、写盘；`svc.status` 的 probe 在事件循环里 `try_wait + sleep(50ms)` 等外部命令。它们都发生在**唯一的 daemon 主循环**上。

影响：

- 一个插件启动慢 30 s，期间所有 MCP `tools/call`、HTTP 管理请求、其他插件的 stdout 帧都得不到处理（控制连接线程只是把请求塞进队列后等待）。
- 80 个插件逐个安装/启动时，管理面没有“部分成功/进度”；CLI 只能一个命令等一个。
- 服务器场景下，任何一次安装/升级/探测都会成为全局停顿。

### 3.3 D3：连接与线程模型

现状是“一个请求 = 一条管理连接 + 一个连接线程”：

- `channel::serve` 读一行就结束；CLI 一次命令一次连接尚可，但 MCP 桥的每次 `tools/list`/`tools/call` 都走 `channel::request`，即新连接、新的 daemon 侧线程、重新读 token 文件、重新握手（`src/mcp.rs:278-302`、`src/channel.rs:139-163`）。
- HTTP 管理 API 的每个请求也在 `api::handle` 里通过 `channel::request` 走一次管理连接，于是**一次 HTTP 请求至少两条连接、两个线程**（`src/api.rs:246-249`、`:892-1011`）。
- 每个 notify 插件固定两条 reader 线程（stdout/stderr，`src/slot.rs:333-358`、`:383-413`），本次 80 插件实测 163 线程。
- 性能报告已指出高并发 p99 的长尾主要来自 thread-per-connection 和突发连接（`docs/research/2026-11-13-core-compat-and-performance.md`）。

在 80 个插件、门户轮询/多用户并发下，这不是“再调大线程栈”能解决的；需要**持久连接 + JSON-RPC id 多路复用 + 连接池 + 有界工作线程**。

### 3.4 D4：没有真正的“按需启用”

把四个概念拆开看，当前只有一个半：

| 概念 | 当前实现 | 缺少 |
|---|---|---|
| 已安装 | 有（`Registry`） | 无“目录可见性/授权” |
| 开机自启 | 有（`enabled` 标记） | 无“按用户/租户”的启用 |
| 运行中 | 有（Slot 状态机） | 无“调用时自动启动” |
| 授权 | 无（只有全局管理员密码） | 无 scope/grant |

所以“工具门户按用户需求启用插件”只能由门户自己维护一套映射：用户点击 → 调 `svc start` → 再调工具。但：

- MCP 端点仍然把所有工具暴露给持有 Bearer 的任何人，门户的过滤可以被绕过；
- 客户端要先知道并调用 `<id>.start`；MCP 工具面把 `start` 当成普通工具；
- 启动后没有空闲回收；80 个插件被逐个点开后会长驻；
- `enabled` 是全局的，一个用户点“启用”会影响所有用户和下次开机。

### 3.5 D5：权限与插件 UI 隔离

`lan.rs` 的会话 cookie 是 `Path=/`；`/plugin-ui/{id}/rpc` 和 `/plugin-ui/{id}/events` 都接受这个全局会话，并把 URL 里的 `{id}` 当作目标插件（`src/api.rs:609-620`、`:698-732`）。`webui::bridge_request` 只做“给 name 加 `{plugin}.` 前缀”，不校验调用方页面属于哪个插件。

因此插件 A 的页面（同源、带会话 cookie）可以直接 `fetch('/plugin-ui/B/rpc', {method:'POST', body:...})` 调 B 的能力（当 B 有 UI 且在运行）；SSE/WS 同理可以订阅 B 的通知。**插件的签名只证明来源，不证明它应该读另一个插件的数据。** 在单插件场景这只是小问题；到“插件门户/多插件”就是跨插件数据访问漏洞。

此外：

- `/mcp` 只有一个 Bearer 密码，没有用户、scope、租户、审计；
- 本地管理 token 是组可读文件（`0640`），同组进程可以控制全部插件；
- Windows 命名管道创建时安全描述符为 `NULL`（`src/channel.rs:309-320`），研究文档已指出默认 DACL 会给 Everyone 读权限（`docs/research/2026-11-12-plugin-ipc-and-reverse-proxy.md`）；
- 插件进程继承 svcd 权限，规格 #19 仍未关闭。

### 3.6 D6：准入、超时与熔断

每插件 64 在途是好的局部保护，但没有：

- 全局在途上限 / 每用户上限 / 每工具上限；
- 有界等待队列与公平调度（第 65 个直接 `busy`，不是排队）；
- deadline 传播；超时只是给插件发 `notifications/cancelled`，插件可以忽略，宿主不强制停；
- 健康检查/熔断：插件卡住但 stdout 还开着时，后续调用继续打到它；
- 幂等键/去重：超时后客户端重试可能重复副作用。

80 个插件 × 64 = 5,120 个潜在在途；每个在途都占一个连接线程和一个 `PendingCall`。服务器规模下必须有“总闸门 + 队列 + 公平 + 熔断”。

### 3.7 D7：资源隔离

`proc.rs` 的 Job Object 只设置了 `KILL_ON_JOB_CLOSE`（杀进程树），没有内存/CPU 限制；Linux 侧只有 `PR_SET_PDEATHSIG`，没有 cgroup/UID 降权。插件和 hooks 都以 svcd 的权限运行（规格第 10 节也把“插件默认继承 root/System”列为 #19）。

对一个“工具门户”来说，这意味着：

- 一个插件泄漏内存/刷日志/开线程，会影响同机所有插件和宿主；
- 无法给不同用户/租户做资源配额；
- 插件读宿主配置、token、其他插件数据没有 OS 边界。

### 3.8 D8：服务器规模

当前没有：

- 远端 agent 注册/心跳/目录汇总；
- 插件 placement（装到哪台 cell）；
- 调用路由（网关按插件 id 找到 cell）；
- 目录的持久化/搜索索引；
- 多副本/故障转移。

单机注册表是 `BTreeMap`（`src/registry.rs:181-204`），boot 时从磁盘扫描（`src/plugin.rs:450-499`）。这意味着“插件数量不限”只能靠：
- 单机继续堆进程/线程（有上限）；
- 或者外部工具门户自己维护多台 svcd 的列表并逐个转发（没有统一目录、没有健康、没有一致的工具名）。

后者可以临时做 demo，但不是产品级“服务器数量不限”。

### 3.9 D9：目录契约与元数据

`package::validate_manifest` 对能力的校验只有：非空、不以 `svc.` 开头、不能叫 `start`、不能重复（`src/package.rs:471-486`）。没有：

- 工具名长度/字符集（MCP 建议 1–128，字符集 `[A-Za-z0-9_.-]`）；
- 单插件能力数上限、单 schema 体积上限、总目录体积上限；
- `title`、`icons`、`annotations`、`outputSchema` 等门户需要的元数据；
- 输出 schema 与结构化结果契约（当前只透传插件的 result，不校验 `structuredContent`/`outputSchema`；门户无法依赖稳定的结构化结果）。

一个恶意/低质量插件可以：
- 用超长名字/超大 schema 让 `tools/list` 膨胀；
- 用重复/相似名字污染模型选择；
- 返回非对象结果，被包装成 `isError`；
- 让门户无法渲染分类、图标、风险提示。

### 3.10 D10：热升级时工具面与实际进程不一致

`install_plugin` 在 `replace` 策略下会先停运行中的插件；但 `keep-old` 策略允许“旧版本继续跑，装新版本，下次 start 用新版本”。代码在 `still_running == true` 时**不更新 Slot 的 exec，也不重建 Slot**，随后把 Registry 里的 Service 换成新 manifest（`src/daemon.rs:807-850`）。

结果：`is_live` 仍为 true，`tools/list` 会按**新 manifest 的 caps** 展示，而实际进程是旧版本。新能力可能调不通，旧能力可能已经不存在。对工具门户/自动升级来说这是“看起来有、调用失败”的典型故障。

### 3.11 D11–D13：运维与协议表达力

- **D11 可观测性**：没有 per-tool 指标、没有审计、没有 trace；只有 `svcd.log` 和每插件 1 MiB×3 的日志。门户需要“谁在何时调了什么、耗时、结果、失败原因”。
- **D12 协议表达力**：固定 30 s、1 MiB 参数/结果、无进度/流式/blob、无反向 RPC（插件不能请求宿主做日志/采样/配置/调用其他工具）；插件结果虽可返回对象，但没有 `outputSchema`/`structuredContent` 校验，富结果（图片/大文件/结构化数据）也没有标准出口。长任务（构建、部署、文件处理）和富结果做不了。
- **D13 启动/探测**：boot 全量扫描 + 全量 Slot；`start_enabled` 按层等待；status probe 在事件循环里最多 5 s/个。插件数大时，启动时间和 `svc status` 都会线性变差。

## 4. 处理方案

### 4.1 设计原则

1. **保留执行核，分离“目录/准入”和“执行”。** 状态机、Slot、NDJSON、签名安装不动；把工具目录、过滤、激活、路由放到独立模块，后续可搬到网关。
2. **单机也按“边界”写。** 20/80 不需要额外进程，但代码边界要是“Edge（目录/准入/路由）→ Supervisor（Slot）→ Plugin”；服务器版只是把 Edge 换成独立网关。
3. **控制面永远不阻塞。** 任何可能超过几十毫秒的动作（验包、解包、启动等待、stop hook、status probe、扫描）都进 worker/job；事件循环只做状态推进。
4. **工具目录是“有版本、可分页、可过滤的索引”，不是每次现算的 JSON。**
5. **启用 = 安装 + 授权 + 激活策略，不是单一 `enabled` 文件。**
6. **服务器规模用 cell 而不是巨型 daemon。** 每个 cell 有明确插件上限和资源预算；网关无状态横向扩。

### 4.2 目标架构

**小规模（嵌入式 20 / PC 80）：单二进制，内部模块化**

```text
MCP 客户端 / 门户 / CLI
        │
┌───────▼──────────────────────────────────────────┐
│ svcd                                             │
│  Edge：认证 / 目录视图 / 分页 / 激活 / 路由 / 配额 │
│  Catalog：快照 + 版本 + 搜索 + 每用户过滤         │
│  Scheduler：lazy start / warm pool / idle stop    │
│  Supervisor：Machine + Slot + Transport           │
│  Control：job queue + worker pool                 │
│  Transport：持久管理连接 + stdio NDJSON           │
└───────┬──────────────────────────────────────────┘
        │ stdio
   plugin × N（每插件受限进程）
```

**服务器规模：无状态网关 + 有界 cell/agent**

```text
        MCP 客户端 / 工具门户（OIDC / API token）
                        │
        ┌───────────────▼────────────────┐
        │ Gateway（无状态，N 副本）       │
        │  authn/z、租户、scope、配额     │
        │  catalog 索引（DB/搜索）        │
        │  分页/过滤/搜索、工具 ACL       │
        │  路由到 cell、健康、重试/熔断   │
        └───────┬────────────────────────┘
                │ mTLS（内部 RPC 或 MCP）
     ┌──────────┼───────────┐
     ▼          ▼           ▼
  cell/agent  cell/agent  cell/agent   （每个跑有界数量插件）
     │          │           │
  plugin...  plugin...   plugin...
```

关键点：

- **cell 的大小是运维参数**（例如 32/64 个插件），不是“无限”。插件数量不限靠增加 cell/主机，不是把单 daemon 调大。
- 网关不持有插件进程；插件目录、调用路由、授权在网关；cell 只信任网关（mTLS），不需要再实现一遍用户认证。
- 对 20/80，Edge 和 Supervisor 在同一进程；对服务器，Edge 抽出去，Supervisor 复用同一个 `svcd`。
- 如果不想做独立网关，最低成本的服务器版是“工具门户自己当网关”，但它仍然需要 D1/D4/D5 的目录、激活、授权能力；长期不建议把多机逻辑散落在门户里。

### 4.3 P0：先让 80 个插件和工具门户可用（建议 2–3 周）

#### P0-1 目录快照 + 分页 + 体积预算

- 引入 `CatalogVersion`：Registry 变更、manifest 变更、Slot 状态变化时递增；每个版本缓存已序列化的工具页。
- `tools/list` 支持 `cursor`/`limit`：稳定排序、opaque cursor、每页默认 100、上限 200；`nextCursor` 只表示“还有”。
- `tools/search`（或门户 API）支持按 `q`、`tag`、`plugin`、`category` 过滤；管理面保留“完整目录”的只读视图。
- **默认不把 `<id>.start` 放进普通 `tools/list`**：生命周期改用“调用时激活”（见 P0-4/P1-1）。这会修改 ADR 0001 的“死的只暴露 `<id>.start`”；建议保留 `includeStartTools`/旧版开关，默认关，并写一条新 ADR 记录这个语义变化。
- 安装期校验并限制：工具名 1–128 + 字符集、每插件能力数（例如 ≤64）、单 schema（例如 ≤16 KiB）、单插件目录（例如 ≤256 KiB）、全局目录页大小。超限拒绝安装，而不是运行时炸。
- 管理通道和 MCP 桥都加“响应超限”保护：分页后正常不会超；如果单页仍超，返回明确的 `-32603`/`isError`，不要让 `write_frame` 直接杀进程。
- `registry.version` 改成 O(1) 的 `CatalogVersion` 计数，不再每次哈希整个列表。

验收：

- 80 个插件 × 20 个中等 schema 能力：`tools/list` 分页成功，每页 <64 KiB；不存在 1 MiB 失败。
- 同一目录重复请求命中缓存，p95 不随插件数线性增长。
- 工具名/schema 超限的包在安装期被拒绝。

#### P0-2 控制面非阻塞（job + worker）

- 增加 `Job` 概念：`install` / `start` / `stop` / `probe` / `uninstall` 都返回 job id；Daemon 事件循环只接收 `JobStarted/JobProgress/JobDone`，不再 `sleep` 等待。
- 现有的 `PendingCall.reply: Sender<String>` 已经支持“稍后回复”，把 `start/stop/install` 也改成 deferred reply；CLI/HTTP 连接线程等待 job 结果，事件循环继续服务其他请求。
- 重活交给固定大小 worker pool：包验签/解包、hook、解释器探测、status probe、目录索引构建。worker 数量有上限（嵌入式 2、PC 4、服务器可配）。
- 定时器改成“最近 deadline”等待（condvar 等到下一个 timer 或 shutdown），不再固定 50 ms tick；`pump` 用稳定索引遍历，避免每轮 `BTreeMap::remove + insert`。
- `start_enabled` 变成 boot 期提交一批 start job，按层并行、按 job 推进，不阻塞管理面开始处理请求；系统服务管理器的 READY 仍可在管理面可用时发送。

验收：

- 一个插件故意 30 s 才 ready 时，另一个插件的 `tools/call`、`svc list`、HTTP `/api/v1/services` p99 仍 <100 ms。
- 80 个插件同时 start 时，管理面可查询进度；不会出现“所有请求一起卡住”。
- 安装 512 MiB 包时，事件循环仍可响应 ping。

#### P0-3 持久复用管理连接

- `channel::serve` 改成循环读帧，直到 EOF；认证一次，后续帧复用；支持多请求在途（JSON-RPC id 复用）。
- 客户端增加持久连接封装：MCP stdio 桥启动时建一条连接，所有 `registry.tools`/`registry.call` 复用；CLI 仍可一次一连接；HTTP 用小型连接池（例如 8 条）。
- token 只读一次；连接断开时重连。
- 增加每连接在途上限与总连接上限；超限返回 `-32000 busy`/429，不无限建线程。
- 对 `tools/call` 的 deferred reply，连接层要支持“响应乱序返回”（按 id 匹配），不能假设一问一答。

验收：

- MCP 桥连续 1,000 次 `tools/call`：管理连接数保持 O(1)，线程数不随请求增长；p95 比现在明显下降。
- 80 插件空闲时线程数接近 `2N + 固定池`，不因 UI/HTTP 轮询增长。
- 管理连接半开/断开不会泄漏线程。

#### P0-4 全局准入、取消与熔断（最小版）

- 全局在途上限（例如 256）、每插件上限（保留 64 或按 manifest）、每用户上限（有用户后）；有界队列 + deadline；队列满返回 429/`busy`。
- 每个请求带 deadline；超时先发 `notifications/cancelled`，宽限期后仍无响应则把插件标记为不健康/重启（可配置）。
- 健康检查：Running 后周期性 ping（例如 30 s，可关）；连续失败进入 Backoff/Failed；工具列表随状态撤回。
- 熔断：连续 N 次调用失败/超时，暂时隐藏该插件的非 start 能力并返回明确错误，避免继续打爆。

验收：

- 100 个并发调用打向一个卡住的插件：其他插件调用不受影响；卡住插件被隔离/重启；总在途不超过上限。
- 超时后插件侧的副作用有明确“可能已执行”的语义（至少日志/审计），不静默重试。

#### P0-5 插件 UI 按插件隔离（安全修复，越早越好）

- 插件页面使用**按插件签发**的会话：token 里带 `plugin_id`，cookie `Path=/plugin-ui/{id}/`；`/rpc`、SSE、WS 都从 token 取插件 id，而不是信任 URL 里的 id。
- 管理控制台的会话与插件页面会话分开；插件页面不能调用 `/api/v1`。
- `/plugin-ui/{id}/rpc` 只允许 `tools/call` 且 name 必须是 `{id}.` 前缀（现有逻辑保留），并校验调用方 token 的 plugin id。
- 长期：每个插件独立 origin（子域/端口）是更彻底的隔离；在单端口反代下，至少做到按插件 token + path + Origin 校验。

验收：

- 插件 A 页面调用 `/plugin-ui/B/rpc` 返回 403；订阅 B 的 SSE/WS 返回 403。
- 普通用户拿不到管理员 API；插件 UI 不能改服务状态。

#### P0-6 回归与门禁

- 新增目录规模测试：0/20/80/200 个插件 × 可配置能力数/schema 大小；断言 `tools/list` 分页成功、单页字节上限、总耗时。
- 新增“控制面不被阻塞”测试：慢启动插件 + 并发 ping/list；断言 p99。
- 新增“插件 UI 跨插件”测试：A 调 B 必须 403。
- 在 CI 加目录体积/页大小的静态门禁（不必跑完整 200 插件，可用合成 Registry 单测）。

### 4.4 P1：按需启用与工具门户（建议 4–6 周）

#### P1-1 Activation 策略

在 manifest 增加（建议 `schema: 3`，向后兼容）：

```json
{
  "activation": {
    "mode": "lazy",          // eager | lazy | manual
    "idleTimeoutSec": 300,
    "keepWarmSec": 60,
    "maxConcurrent": 8,
    "startTimeoutSec": 15
  },
  "limits": { "memoryMiB": 128, "cpuPercent": 25 }
}
```

语义：

- `eager`：boot 时启动（现有行为，默认给老插件）。
- `lazy`：平时不启动；第一次 `tools/call` 时自动 start，等 ready 后转发；空闲超过 `idleTimeoutSec` 且无在途则 stop。
- `manual`：只能显式 start（现有行为）。
- `keepWarmSec`：激活后至少保持存活的时间（避免刚启动就因短暂空闲被回收；与 `idleTimeoutSec` 取较晚者）。
- 工具是否出现在目录：`lazy` 仍出现（因为调用可激活），但目录项带 `activation: lazy` 元数据；`manual` 默认不出现，除非管理面/显式查询。
- start 工具不再作为 MCP 工具暴露；生命周期走宿主内部激活。兼容期保留开关。

调度：

- 激活请求进有界队列，全局并发启动数受限（例如 4），避免 80 个调用同时拉起 80 个进程。
- 激活与普通调用共享 deadline；启动失败返回结构化 `isError`（含原因/重试建议）。
- 空闲 reaper 每 30 s 跑一次；`enabled` 的 eager 插件不自动停；有在途/用户会话绑定的不自动停。

验收：

- 1,000 个已安装、50 个活跃：目录可搜索，活跃集内存有界；idle 插件在 5 分钟后自动停止。
- 冷调用 p95 <2 s（本地），热调用接近现有水平。
- 80 个并发冷调用不会把 daemon 打满（启动并发受限）。

#### P1-2 用户、租户与授权

- 认证：门户用 OIDC/OAuth 2.1（MCP 授权规范）或至少 API token + 用户库；本地 CLI 继续用本地 token；管理员/普通用户/服务账号分角色。
- 授权：`user/tenant -> plugin/tool -> call` 的 grant 表；`tools/list` 按请求授权过滤（MCP 2026-07-28 明确允许工具集合随授权变化）；调用未授权工具返回 `-32602`/403，不泄露存在性。
- “启用”对用户来说 = grant + 是否出现在目录，不是进程状态；进程状态由 activation 管。
- 审计：记录 user、tenant、tool、参数摘要（脱敏）、结果、耗时、错误、job id；可按用户/插件查询。
- 配额：每用户 QPS/并发/每日调用量；超限 429。

验收：

- 两个用户看到不同的工具列表；A 无法调用 B 的插件（即使知道工具名）。
- 管理员能审计“谁在何时调了什么”；日志可导出。
- MCP 的 Bearer/OAuth token 能映射到用户与 scope。

#### P1-3 资源隔离与健康

- Linux：cgroup v2（memory.max、cpu.max、pids.max）或 systemd slice；每插件独立 UID 或至少 no-root；必要时 seccomp/landlock 最小集。
- Windows：Job Object 增加内存/CPU/进程数限制，不只 `KILL_ON_JOB_CLOSE`。
- 嵌入式：没有 cgroup 时至少限线程/文件/日志速率，并在 manifest 声明资源预算；超预算拒绝启动。
- hooks：降权运行、固定超时、限制环境变量；默认拒绝（嵌入式已有）。
- 日志：per-plugin 速率限制/采样；总日志磁盘配额。
- 健康：per-plugin RSS/线程/重启次数/工具错误率；异常插件隔离。

验收：

- 一个插件分配 1 GB 内存/占满 CPU 时，其他插件和宿主仍可用；超限插件被限制或杀死并记录。
- 80 插件空闲 RSS/线程在预算内；日志不把磁盘写满。

#### P1-4 目录元数据与门户 API

- Manifest 能力增加 `title`、`icons`、`tags`、`category`、`annotations`（readOnly/destructive/idempotent/openWorld）、`outputSchema`、`timeoutSec`。
- `tools` 返回结构化 `structuredContent`（按 outputSchema），保留 text 兼容。
- 门户 API：目录搜索、分类、最近使用、收藏、按插件/用户过滤、调用历史；这些不应逼 MCP 客户端拉全量。
- 工具名规则与去重：插件 id + `.` + 能力名；冲突时网关加 cell/租户前缀（MCP 规范建议代理做 disambiguation）。

### 4.5 P2：服务器数量不限（建议 2–3 个月，按业务启动）

1. **Agent 注册与心跳**：每个 cell 启动时向网关注册（id、地址、容量、已装插件、版本、健康）；心跳/租约；离线摘除。
2. **目录索引**：安装/升级时写持久目录（Postgres/搜索索引），网关从索引提供分页/搜索/过滤；不再依赖每次 RPC 全量构建。
3. **Placement**：安装时选 cell（按容量/亲和/租户隔离）；插件 id 全局唯一；迁移/重建策略明确。
4. **路由**：`tools/call` → 网关按 `plugin_id` 找 cell → 内部 RPC/MCP → cell 的 `svc.call`；网关做超时、重试（仅幂等）、熔断、限流。
5. **无状态网关**：会话/目录缓存放 Redis/DB；网关副本可水平扩；MCP 端点按租户/用户分片。
6. **联邦（可选）**：网关作为 MCP 客户端接入外部 MCP server，把远端工具纳入目录；工具名加前缀；健康/配额/审计一致。
7. **多租户**：租户级目录、配额、网络策略、插件隔离；跨租户默认不可见。

验收：

- 10 个 cell × 200 个已安装插件（约 2,000 目录项），网关分页/搜索 <200 ms；单 cell 故障不影响其他 cell 的调用。
- 扩容 cell 不需要改客户端；工具名稳定。
- 网关重启后目录/路由恢复；调用不丢（幂等工具可重试，非幂等返回明确状态）。

### 4.6 协议与兼容演进

- **保持 NDJSON JSON-RPC**：嵌入式和单机继续用；只加字段/方法，不换框架。
- **`.spk` schema 3（加法）**：`activation`、`limits`、能力 metadata/outputSchema、每能力 timeout；老 daemon 拒绝 schema 3（现有 schema gate 机制），新 daemon 兼容 1/2。
- **工具列表**：默认分页；`includeStartTools` 兼容开关；`tools/search` 自定义方法；现代 MCP 用 `nextCursor`/`ttlMs`，legacy 保持 `listChanged`。
- **调用语义**：每能力 `timeoutSec`；请求带 deadline/trace/user；进度用 `notifications/progress`；取消用 `notifications/cancelled` 并要求宽限后强制；结果支持 `structuredContent` 和 blob 引用。
- **大对象**：定义 blob store（内容寻址），参数/结果超过阈值用引用；不要简单把 1 MiB 调到 100 MiB。
- **反向 RPC（P2）**：如果插件需要“请宿主读配置/发日志/调另一个工具/请求用户确认”，定义宿主方法白名单和权限；当前“插件只能回响应/通知”会限制复杂工具。
- **版本协商**：`protocol` 目前是 1 且无握手；可在 schema 3 加能力版本/特性位，但不要破坏“未知字段拒绝”的签名契约。

### 4.7 不建议做的事

- **只把 1 MiB 限制调大**：能绕过当前报错，但全量目录仍然会撑爆模型上下文、内存和延迟；必须分页/过滤。
- **把所有插件塞进一个进程**：省内存但丢隔离，一个插件崩溃/泄漏会带走全部；嵌入式真要省，可另做 WASM/受限 in-process 运行时，而不是取消进程模型。
- **在嵌入式核心里引入 async runtime/axum**：体积和复杂度不划算；网关可以用独立二进制/feature。
- **让每个插件自己起 HTTP**：现有反代模型更适合统一鉴权/路径/证书；问题在隔离和目录，不在“要不要反代”。
- **把用户/租户状态放进插件进程**：授权/配额属于 Edge/网关；插件只做业务。

## 5. 分阶段路线与验收

| 阶段 | 时间盒 | 交付 | 退出标准 |
|---|---|---|---|
| P0 | 2–3 周 | 目录分页/缓存/预算；非阻塞 job；持久管理连接；全局准入/取消；插件 UI 隔离；80 插件回归 | 80×20 能力目录可分页；无 1 MiB 失败；慢启动不阻塞其他请求；A 不能调 B；线程/连接不随请求增长 |
| P1 | 4–6 周 | activation/idle/warm pool；用户/租户/scope；资源隔离；目录元数据/搜索；审计/指标 | 1,000 安装/50 活跃；冷调用 p95 <2 s；每用户目录不同；超限插件被隔离；有审计 |
| P2 | 2–3 月 | 网关 + cell；注册/心跳/placement/路由；目录索引；联邦；多租户 | 10 cell × 200 插件；单 cell 故障不影响其他；网关水平扩；搜索 <200 ms |

## 6. 测试与观测建议

- **目录基准**：`tools/perf` 增加 `catalog_scale`（或扩展现有 `scale_cli.py`）：参数化插件数、能力数、schema 字段数；输出每页字节数、页数、p50/p95、失败率；断言单页上限。
- **控制面响应**：慢启动插件（sleep 30 s）+ 并发 `svc list`/`tools/call`，断言管理面 p99；安装大包时 ping 仍通。
- **故障注入**：插件不响应、stdout 乱码、超大帧、无限通知、日志洪水、启动即崩、stop 不退出；断言隔离/重启/不拖垮宿主。
- **安全测试**：A 插件页面调 B；无 token 调 `/mcp`；跨租户工具名猜测；插件提权/读宿主文件；Windows 命名管道 ACL。
- **Soak**：80 插件 24 h；内存/线程/日志/文件描述符增长；legacy 每秒轮询；SSE/WS 重连。
- **指标**：`svcd_tool_calls_total`、`svcd_tool_latency`、`svcd_inflight`、`svcd_queue_depth`、`svcd_plugin_restarts`、`svcd_plugin_rss`、`svcd_catalog_bytes`、`svcd_catalog_version`；按插件/用户/工具维度；审计单独存储。

## 7. 需要拍板的点

1. **“服务器数量不限”是单机插件数不限，还是多机部署数量不限？**
   - 若是前者：单机仍需分 cell/分片，不能一个 daemon 无限堆；若是后者：按 P2 网关 + cell 设计。
2. **用户体系从哪来？** 内置用户库、OIDC、还是复用现有 `auth_passwd` 只做管理员？这决定 D5/P1 的落地成本。
3. **MCP 工具列表按“授权过滤”还是“用户手动选择”？** 规范允许按请求授权变化；门户可能还需要“会话内启用/禁用”的自定义层。
4. **老插件的默认 activation 是 eager 还是 lazy？** 建议先 eager（兼容），新包默认 lazy；否则升级后行为变化。
5. **`<id>.start` 是否保留为 MCP 工具？** 建议默认不暴露，保留兼容开关；否则目录永远被 start 工具污染，且模型会误把生命周期当业务能力。
6. **资源隔离的目标平台**：Linux cgroup v2 / systemd、Windows Job Object、嵌入式是否有 cgroup？没有时的预算策略要定。
7. **网关是独立二进制还是 `svcd` 的 feature？** 建议独立（体积、部署、权限边界都更清晰），但共享 crate/协议。

## 附录 A：本次目录实验的可复现步骤

```bash
# WSL2 Debian；二进制为仓库当前 target/release/svcd（glibc）
# 1) 建临时 config/data/logs/trust，复制 pub key
# 2) 生成 manifest：schema=1, exec.ready=notify, 20 个 capability，
#    每个 inputSchema 含 20 个 string 字段（约 1 KB）
# 3) 生成 agent.sh：
#    while IFS= read -r line; do
#      printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}'
#    done
# 4) 对每个 pN：plugin bundle --id pN --key ... --key-id sample-2026
#                plugin install pN.spk
#                svc start pN
# 5) 用 legacy 初始化 + tools/list 调 mcp：
#    {"jsonrpc":"2.0","id":1,"method":"initialize",
#     "params":{"protocolVersion":"2025-11-25","capabilities":{}}}
#    {"jsonrpc":"2.0","id":2,"method":"tools/list"}
```

实测输出：

```text
plugins=10 mcp_exit=0 bytes=614406
plugins=20 mcp_exit=1 bytes=160  err=mcp: stdout: frame exceeds the 1 MiB limit
plugins=80 mcp_exit=1 bytes=160  err=mcp: stdout: frame exceeds the 1 MiB limit
install+start 80 total_ms=3778
daemon threads=163
```

## 附录 C：2026-11-14 后续实现进展

- **已实现**：MCP `tools/list` cursor 分页（`src/catalog.rs`）、按消息类型拆分的帧上限、本地管理通道有界帧、管理连接复用池。
- **已实现**：Unix 插件 stdout/stderr reactor；80 个插件复测 daemon 线程从 163 降到 4（见 2.2.1）。
- **已实现**：插件每连接/每插件的 frame 队列改为有界 `sync_channel`；reactor 在队列满时暂停该插件读取。
- **已实现**：manifest `protocol: 2` 的 4 字节长度前缀帧；已用 Python 长度帧插件在 WSL 端到端验证 `initialize` / `tools/list` / `tools/call`。
- **已实现**：内容寻址 blob store、`POST/GET /api/v1/blobs`、工具参数中的 `$blob` 解析为受控路径（`SVCD_TEMP_DIR`）。
- **已实现（Unix）**：HTTP 监听器/普通请求进入 poll reactor，阻塞 handler 由 4 个固定 worker 执行；普通 HTTP 连接不再各自创建线程。实测 400 并发 HTTP 请求时 daemon 峰值 25 线程、回落到 4。
- **已实现（Unix）**：SSE 进入 reactor 状态机（订阅、replay、heartbeat、chunked 写出都不再占连接线程）；WebSocket 也进入 reactor 状态机（握手、帧解析、事件转发、分片重组、`tools/call` 交给 worker）。Unix 上普通 HTTP / SSE / WS 都不再占每连接线程。
- **已实现（Unix）**：本地管理 UDS 进入独立 poll reactor；每个连接只是 fd/状态，不再每连接一线程。80 插件、全 reactor 下 daemon 为 **8 线程**（daemon + 插件 reactor + 管理 reactor + HTTP reactor + 4 worker）。
- **已实现**：插件把大结果写入 `SVCD_TEMP_DIR/incoming/`，返回 `$blobPath`；宿主校验路径、流式哈希并落成内容寻址 blob，MCP 结果替换为 `resource_link` + `structuredContent.$blob`。
- **已实现**：MCP `tools/call` 的 progress 透传。客户端在 `_meta.progressToken` 给出 token，宿主把它带给插件；插件发 `notifications/progress` 时，管理通道可以多帧返回，stdio 桥在等待最终响应期间把进度写给客户端。Windows 的线程通道同样支持多帧。
- **已实现**：HTTP MCP progress。`POST /mcp` 在 `Accept: text/event-stream` 且 `tools/call` 带 progressToken 时返回 SSE：progress 事件在前，最终 JSON-RPC response 在后；Unix reactor 和 Windows fallback 都支持。
- **已实现（Unix）**：管理客户端 reactor。HTTP MCP streaming 的上游管理连接由一个 poll 线程多路复用，不再每个流一个阻塞线程；每流一个 fd + 有界队列。
- **已实现**：`SVCD_TEMP_DIR`（原 `SVCD_BLOB_DIR`）与后台清理线程；默认 2 小时 TTL、5 分钟扫描，`temp_dir` / `temp_retention_secs` / `temp_cleanup_interval_secs` 可在 `svcd.conf` 配置。
- **已实现**：插件 blob 约定文档（`$blob` / `$blobPath` / `SVCD_TEMP_DIR`），见 `docs/plugin-blob.md`。
- **尚未实现**：job 化长任务、逐 token 结果流（当前 progress 是通知，不是结果分块）。

## 附录 B：相关代码位置

- 工具投影：`src/mcp.rs:24-53`、`:278-302`
- 1 MiB 分帧：`src/ipc.rs:18`、`:79-95`
- Daemon 主循环：`src/daemon.rs:315-340`
- 同步 start/stop/install/probe：`src/daemon.rs:729-770`、`:943-981`、`:772-863`、`:1096-1152`
- 每连接一线程：`src/channel.rs:81-132`、`src/api.rs:246-249`
- MCP 桥每次新建管理连接：`src/mcp.rs:278-302`、`src/channel.rs:139-163`
- 每插件在途/超时：`src/slot.rs:22-24`、`:142-184`
- 插件 UI 反代与 `/rpc`：`src/api.rs:606-732`、`src/webui.rs:33-74`
- 会话 cookie：`src/lan.rs:21`、`:224-241`
- 注册表：`src/registry.rs:181-204`
- 安装/扫描/能力校验：`src/plugin.rs:450-499`、`:612-743`；`src/package.rs:471-486`
- 进程隔离现状：`src/proc.rs:94-172`
- 规格中未决的权限边界：#19；`docs/specs/2026-11-12-核心层需求最终版.md:251`
