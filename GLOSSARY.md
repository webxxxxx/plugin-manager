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

**未签名安装（Insecure install）**:
只由 `svcd.conf` 的 `allow_insecure_install` 控制的安装模式，默认关闭。开启后普通安装先严格验签，验签失败才降级接受未签名或未信任 key 的包；CLI/HTTP/Web 都没有逐次 `--insecure` 开关或勾选项。`files[]` 哈希与平台/hooks 校验不放宽；日志标注未验签，这样装的版本不能导出为签名 `.spk`。
_Avoid_: 信任安装、跳过校验（默认路径不跳过）

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

**控制通道（Control channel）**:
CLI / MCP / plugin 与 svcd 之间的本地 NDJSON 通道：Unix 走 UDS，Windows 走命名管道；服务端 bind、客户端 connect、连接池与 token 权限由 `ControlChannel` 平台 seam 收口（#143）。与 HTTP / MCP HTTP 面区分。
_Avoid_: IPC（那是插件 stdio 与控制通道的统称）、管理端口

**plugin**:
只管理插件的 CLI；动词与 `svc` 同源，但作用域限定在 `kind=plugin`，另加 `install / uninstall / editconfig / export / import`。
_Avoid_: 插件管理器、PluginManager（那是已退役的进程）

**svcd.conf**:
`<config>` 下的运行时配置文件，由 `svcd --install --conf <file>` 写入；承载 HTTP 端口、`config_dir`、`install_dir`、`auth_passwd` 哈希、LAN 默认开关、plugin 安装位置与 core 服务扫描位置。
_Avoid_: settings.json、通用“配置文件”

**auth_passwd**:
`svcd.conf` 里的 PBKDF2-HMAC-SHA256 口令哈希；明文只经过 `svc auth passwd <password>`，不落盘。网页登录、`/api/v1` 与 `/mcp` 的 Bearer 都用同一密码。
_Avoid_: lan-token、管理员 token

**config_dir**:
`svcd.conf` 里的配置目录覆盖项；`svcd.conf`、`trust.d`、`svcd.token` 都放在这里。默认 Linux `/etc/svcd`、Windows `%USERPROFILE%\.svcd`；`SVCD_CONFIG_DIR` 优先级更高。

**install_dir**:
`svcd.conf` 里记录的安装根：`svcd --install --prefix <dir>` 会把 `<dir>` 写回这里；之后不带 `--prefix` 的重装、运行时 data/logs 的默认值（`<install_dir>/data`、`<install_dir>/logs`）和 `--uninstall` 的清理都以它为准。手工写在 conf 里也有效，`--prefix` 优先于它；未记录时走平台默认。
_Avoid_: 安装前缀（那是命令行 `--prefix`）、数据目录

**manual 安装**:
`svcd --install --manual`：不注册 init 服务，按宿主系统生成 `svcd_daemon.sh`（Unix）或 `svcd_daemon.bat`（Windows），脚本导出 `PATH`、`SVCD_DATA_DIR`、`SVCD_LOG_DIR` 后启动 svcd；启动接入与手动运行时的环境变量由用户负责。
_Avoid_: 无服务模式（那是 `--no-service`）

**mcp**:
multicall 二进制的 MCP 桥入口名；把 MCP 客户端接到常驻 svcd。
_Avoid_: MCP server、stdio server

**multicall 二进制**:
一个可执行文件，按 argv[0]/子命令分派出 `svcd`、`svc`、`plugin`、`mcp` 四个入口。
_Avoid_: 多合一程序、fat binary

**Blob**:
内容寻址的大对象；参数用 `$blob` 引用，结果用 `$blobPath` 交给宿主落盘，控制帧不携带内容。详见 `docs/plugin-blob.md`。
_Avoid_: 文件缓存、附件

**服务契约（Contract）**:
一个服务对外承诺的东西：它有哪些能力、别人怎么调用它（调用信封）、启动/停止/健康检查要满足什么、可能给出哪些失败类别。只要实现同一份契约，本机进程、组合、远程节点与内置实现就是同一种服务。
_Avoid_: 接口、API、插件规范

**调用（Invocation）**:
一次「请某个服务做某件事」：目标能力、参数、超时、预算，以及它带来的结果或失败类别。大内容不进调用信封，只传引用。
_Avoid_: 请求、request（那是协议层的报文）

**完成回调（ReplySink）**:
一次调用的结果交付：成功或失败都通过它一次性送出；调用入口只同步表达「没被尝试」的拒绝，不等待实现完成。
_Avoid_: 返回值、回调函数

**失败类别（Failure class）**:
一次调用失败的种类，至少区分「暂不可用（可重试）」「永久失败」「依赖缺失」「预算或限流」「被取消」。编排据此决定重试、跳过还是中止；各个对外面据此投影成状态码或退出码。
_Avoid_: 错误码、错误信息、异常

**健康（Health）**:
一个服务可被使用的程度：活着（进程在）、就绪（能接受调用）、降级（能接受调用，但实现层已经报过失败）、死亡。健康是**服务级**的，不逐能力统计。
_Avoid_: 状态（那指状态机核的六态）、存活（那只说明进程在）

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
MCP 的 HTTP 传输：单个 `POST /mcp` 端点，每个请求独立、无 session；普通请求返回一个 JSON，带 progress token 的 `tools/call` 用 SSE 逐条返回进度，最终结果仍是同一流里的最后一条 message。
_Avoid_: HTTP MCP server、SSE 传输（SSE 只属于流式 `tools/call`，不是整个传输）

**启动（Start）**:
让一个死服务进入 ready 的准入动作；死服务在可调用面上唯一暴露的能力。
_Avoid_: 拉起、boot、run

**注册表（Registry）**:
svcd 持有的唯一服务清单；`svc list` 与 MCP 工具面都从它投影。
_Avoid_: 服务表、清单、catalog

**状态机核（Machine）**:
6 态纯核；只接收带 pid 的事件、产出动作，不碰进程、时钟或 IO。由监督槽执行动作。
_Avoid_: supervisor、状态机（不带「核」）

**监督（Supervision）**:
daemon 里「一个服务从定义到运行到回收」的那一层：注册表、状态机核、监督槽、运行态记录与接管、对账、生命周期动作。它包含监督槽，但不等于监督槽；也不是「核心层」——那指的是整个上下文，也不是「Core 服务」——那是一种服务类型。
_Avoid_: core、核心模块、核心层（那指整个上下文）

**运行载体（Carrier）**:
一个服务运行资源的唯一所有者：监督槽（`exec`）或 launcher（`impl_ref`），二选一；两者皆无的服务仍被监督与展示，但不可调用。
_Avoid_: 实现、后端

**监督槽（Slot）**:
一个被监督服务的运行时载体：状态机核 + 子进程 + stdio 管线 + 定时器。
_Avoid_: 实例、worker

**接线（Wire）**:
把字节变成消息、把消息变成字节的那一层：帧、消息信封、插件 stdio、raw 与通道。只做编解码与连接，不做路由、不认识领域概念；因为它被多个对外面共用，所以不归任何一个面。
_Avoid_: 协议栈、通信层、protoc

**线格式（Wire format）**:
一条接线上字节的布局约定（行帧、长度帧、二进制帧）；一条接线只有一种线格式。它和「协议代（Era）」不是一回事：Era 说的是消息形态，线格式说的是字节布局。
_Avoid_: 协议（半个词已被 Era 占用）、格式

**服务器（Server）**:
一个协议的会话层：监听、握手、连接生命周期与传输契约（分帧、流式、协议代、进度事件）。它把连接上的每个请求交给对应的对外面，自己不认识任何领域概念；新增一个协议就是新增一个服务器加一组对外面，不改别的服务器。
_Avoid_: 引擎、监听器、endpoint

**对外面（Face）**:
一个协议面对外暴露的端点与投影：它把领域概念与能力面映射成该协议的请求/响应，自己不持有资源、不拥有连接。服务器（server）建立连接，对外面解释连接上的每个请求。
_Avoid_: UI、界面层、接口层

**请求交接（Request handover）**:
服务器把「已解析的请求 + 恰好一次完成槽」交给对外面，自己保留连接、缓冲、deadline 与背压；对外面用内存响应或 sans-IO stream handler 作答，不接触 socket。见 [ADR-0016](docs/adr/0016-s4-request-handover.md)。
_Avoid_: 接管、takeover、handler 调用

**插件 stdio 反应器（Plugin stdio reactor）**:
Unix 上拥有插件管道的那个 poll 集：通知型插件的 stdout/stderr、exec 型插件的日志、协议 3 插件的 stdin/stdout/stderr 都由它读写，所以每个插件不再自带线程。它没有自己的线程——supervisor loop 在自己的循环里驱动它（`Reactor::poll_once`），注册与唤醒走命令通道加 self-pipe。超时/背压由每条管道自己的队列决定，不是线程阻塞。反应器不存在（或非 Unix 构建）时才退回「每插件读写线程」的降级路径。
_Avoid_: IO 线程池、epoll 管理器、反应器线程

**raw 传输（Raw transport）**:
协议 3 插件与 UI 之间的二进制通道：插件 stdin/stdout 上是 `u32 长度 + u32 stream_id + payload` 帧，UI 侧是 `/plugin-ui/{id}/{token}/raw` 上的 binary WebSocket。daemon 只做 stream 映射、全局在途预算与失败分类，不解析 payload。非 Unix 构建没有这条 WebSocket 桥（匿名子管道不可 poll），`/raw` 明确回 `501 not_supported`；协议 3 插件本身仍能启动，只是没人能订阅。
_Avoid_: 二进制插件、直通 WebSocket

**重启策略（restart）**:
服务退出后怎么办的声明：`always`（任何退出都重启）、`on-failure`（仅失败退出重启，默认）、`never`（不重启，退出即 stopped）。它和 `enabled`（开机是否启动）是两个独立维度。
_Avoid_: 自启、enabled、看护策略

**运行态记录（Runtime record）**:
`<data>/run/<id>.json`：一个服务上一次已知的状态、pid 与 pid starttime。写入时带 boot id，用于 daemon 重启后校验并接管存活进程；不是第二份状态真相，接管后仍以监督槽为准。
_Avoid_: 状态文件、pidfile、快照

**接管（Adoption）**:
新 daemon 通过运行态记录确认「进程是上一代的、还活着、没被 pid 复用」之后直接接回监督，而不是重启它。Windows 上由 Job Object 随 daemon 终止进程树，因此只接管「完成态」（无进程）；已经跑完的一次性服务在所有平台都从记录恢复，不重跑。
_Avoid_: 恢复、重启、重连

**一次性服务（One-shot / RemainAfterExit）**:
启动动作成功退出后效果仍然存在的服务（建隧道、挂载）。成功退出后保持 `running`、`pid` 为空；`svc start` 重跑，`svc stop` 只跑停止动作。core JSON 用 `remain_after_exit` 显式声明；drop-in shell 脚本在 `restart != always` 时沿用 shell 约定。
_Avoid_: 守护脚本、短命服务

**流（Flow）**:
一组步骤及其依赖关系，加上每步的输入输出映射；它是一份可被包装成服务的编排定义。
_Avoid_: 工作流、pipeline；也不要拿「编排」当名词用——编排是动作，流是定义

**复合服务（Composite service）**:
用一条流实现的服务。对监督、注册表与各个对外面来说，它与普通服务没有区别，因此可以被再次组合。
_Avoid_: 复合插件（会被误读成一种新的 `kind`）

**触发器（Trigger）**:
让一条流在某个时刻或某个事件上运行的规则（定时、事件、手动）。
_Avoid_: 定时任务、cron、调度

**汇合（Fan-in）**:
多个并发调用如何合成一个结果：全成功、任一成功、法定数、尽力而为。
_Avoid_: 聚合、join、reduce

**放置（Placement）**:
决定一个服务实例在哪个节点上运行。
_Avoid_: 调度（「调度」同时会被触发器占用，两个意思一律不用这个词）

**兼容标识（device / compat）**:
设备侧 `<config>/compat.conf` 里的单一 `device = <id>`（如 `sr6110`、`sr6110-v2`）；id 标识「内核/固件兼容代」，不是型号。插件在 schema 3 的 manifest 用 `compat.devices` 列出支持的 id：不写即通用，写了必须精确命中。含 `.ko` 的包必须声明。
_Avoid_: 设备型号、arch、ABI 版本号、白名单

**reload（`svc reload`）**:
重新扫描 `services.d` 与插件目录并对账：新增的定义按 `enabled` 启动，删除的定义停掉并遗忘，修改的定义在下次 start 生效。daemon 也会每隔 `SVCD_RESCAN_INTERVAL_MS`（默认 5s）比对目录指纹自动做同一件事；`svc reload` 是手动立即触发。它不是 restart，不碰运行中的进程。
_Avoid_: refresh、rescan（作为用户命令时）