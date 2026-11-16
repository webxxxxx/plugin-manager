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

**插件 stdio 反应器（Plugin stdio reactor）**:
Unix 上唯一拥有插件管道的那条 poll 线程（`svcd-reactor`）：通知型插件的 stdout/stderr、exec 型插件的日志、协议 3 插件的 stdin/stdout/stderr 都由它读写，所以每个插件不再自带线程。超时/背压由每条管道自己的队列决定，不是线程阻塞。反应器起不来（或非 Unix 构建）时才退回「每插件读写线程」的降级路径。
_Avoid_: IO 线程池、epoll 管理器

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

**兼容标识（device / compat）**:
设备侧 `<config>/compat.conf` 里的单一 `device = <id>`（如 `sr6110`、`sr6110-v2`）；id 标识「内核/固件兼容代」，不是型号。插件在 schema 3 的 manifest 用 `compat.devices` 列出支持的 id：不写即通用，写了必须精确命中。含 `.ko` 的包必须声明。
_Avoid_: 设备型号、arch、ABI 版本号、白名单

**reload（`svc reload`）**:
重新扫描 `services.d` 与插件目录并对账：新增的定义按 `enabled` 启动，删除的定义停掉并遗忘，修改的定义在下次 start 生效。daemon 也会每隔 `SVCD_RESCAN_INTERVAL_MS`（默认 5s）比对目录指纹自动做同一件事；`svc reload` 是手动立即触发。它不是 restart，不碰运行中的进程。
_Avoid_: refresh、rescan（作为用户命令时）
