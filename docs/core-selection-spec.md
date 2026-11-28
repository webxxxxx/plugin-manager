# 核心层技术选型规格

> 状态：决策层定稿（2026-11-12）
> 依据：地图「核心层技术选型」的 26 张已关子票、8 份研究笔记、ADR 0001
> 范围：svcd / svc / mcp 核心层；不含商城、ipk2spk、AI 建插件流水线
> 用法：§1–§12 是规格；**§13 是冲突与缺口，交接前必读**

## 0. 一句话

一个 Rust 写的 multicall 二进制，链接出 `svcd`（守护进程）/ `svc`（服务 CLI）/ `plugin`（插件 CLI）/ `mcp`（MCP 桥）四个入口；插件是 `.spk` 包，由 svcd 以 root / System 身份监督；核心层不链 TLS、不打包 WebView 引擎，嵌入式 ≤ 2,000,000 B、桌面 ≤ 30,000,000 B（均指核心二进制）。

## 1. 系统模型

### 1.1 交付物

- **一个 crate、一个二进制、argv[0] 派发**；安装时建 `svcd` / `svc` / `plugin` / `mcp` 四个硬链接，失败降级 symlink，再降级 copy 并告警。
- 桌面另有 `svc ui` 子命令作为 GUI 入口，不单独占入口名。
- **PluginManager 不再存在**：不是进程、不是服务、不是保留名。插件管理是 svcd 的内部功能。
- `svcd run --foreground` 供服务管理器拉起；`svcd install-service / uninstall-service` 向宿主注册自身（systemd / launchd / SCM / BusyBox inittab / procd 五份计划）。
- `svcd start` / `stop` / `status` 按宿主实际 init 分派，不写死 systemd：`/etc/init.d/svcd` 存在走 procd 脚本，有 `systemctl` 走 systemd，只认 `/etc/inittab` 的 `::respawn:` 或 `--manual`（没有注册）时不再调用 `systemctl`——`status` 改问控制通道，找不到 `svcd.conf` 时报 `unknown` 而不是 `stopped`（#286）。
### 1.2 服务与能力面

- 每个服务有 id、kind（core / plugin）、生命周期、依赖与能力面。注册表 = 内置 `svc` + `services.d/*.json` 的 core + `<data>/plugins/*/current` 的 plugin；没有第二份清单。core 与 plugin 的来源、信任、安装方式见 §1.6。**能力面只归 plugin**：core 是生命周期单元，不参与 `tools/call`。
- `svc list` 与 MCP 的 `tools/list` **同源**：死的服务只暴露 `<id>.start`；活的才把 manifest 声明的能力放进工具面。
- `start` 同步到 ready；插件崩溃即摘能力，不自动启动（只有 enabled 的开机启动会重试）。
- `svc.` 命名空间保留给核心能力；插件能力对外是 `<serviceId>.<cap>`。

### 1.3 监督器

- 状态机：`Stopped | Starting | Running | Stopping | Backoff | Failed`，事件带 pid，迟到事件按 pid 丢弃。
- 退避：曾在 Running ≥ 10s → 清零 + 立即重启；否则 1/2/4/8/16/30s；连续 5 次进 `Failed`，只有显式 start 复位。
- 就绪：默认 probe-on-spawn（发首个协议请求，首个合法响应 = Ready），`exec.ready = "exec"` 可跳过；`exec.ready = "port"`（配 `exec.port`，可选 `exec.probe_addr`，默认 `127.0.0.1`）由宿主连一次 loopback，能建连才算 Ready——适合「起了但还要等端口才真能接流量」的守护进程，只对 TCP 有效；start 超时 30s。spawn 前先 bind 预检端口是否已被别人占着；这是**探测不是保证**：预检与 connect 之间仍有 TOCTOU 窗口。
- 停止：SIGTERM（进程组 + 停止时快照到的 `setsid` 子孙）→ 10s → SIGKILL → 5s → 记 unkillable；全局关机预算 30s，unit 侧 `TimeoutStopSec=40`。Windows：控制帧 + 关 stdin → 等 → `TerminateJobObject`（整个服务树）。
- **服务树 = 进程组 ∪ 停止时快照的 `setsid` 子孙**（#332，ADR-0031）：Unix 每次 spawn 建独立进程组，Windows 每个服务一个 job object；`setsid` 出去的子孙不在组里，停止开始的那一刻按 `/proc` 父链快照一次，之后对组与快照一起 SIGTERM/SIGKILL。树里任一进程仍存活时服务保持 `Running`，leader 退出不触发重启；整棵树退出后才按 leader 的退出码进入重启策略。`notify` 类服务的 stdout EOF 仍按控制端点消失处理（立即停止）。
- **父进程死亡（2026-11-13，测试覆盖）**：svcd 异常退出（kill -9 / TerminateProcess）时子进程随父退出——Windows 用 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，所有被监督进程加入该 job），Linux 在 child 里 `prctl(PR_SET_PDEATHSIG, SIGTERM)`；重启后按 enabled 重新拉起，不会再与上次运行的实例并存。macOS 没有等价的廉价原语，仍是已知缺口，待后续票。
- 依赖：按层拓扑，同层并行、层间串行；依赖失败不级联停，只摘能力并拒绝新的 start；依赖恢复不自动拉起之前被挡住的依赖者。
- 环：安装与加载都拒绝，带环路径。

### 1.4 依赖

- `dependencies` 是服务 id 列表，**v1 不做版本约束**；任意已安装版本即满足。
- 安装允许先装依赖者；`start` 按拓扑拉起已安装的依赖，缺依赖才拒绝并列出 id。
- 升级/卸载不阻止、不级联停、不自动重启依赖者；能力面按 §1.2 摘除/恢复。
- 版本兼容由 manifest 的 `protocol` / `platform` 与插件自身负责；未来要范围再加 `id@range`，加法兼容。

### 1.5 服务安装

- 五份 `InstallPlan`：systemd（`/etc/systemd/system/svcd.service`，Type=notify、Restart=on-failure、TimeoutStopSec=40）、launchd（`/Library/LaunchDaemons/com.webxxxxx.svcd.plist`，KeepAlive、ThrottleInterval 10）、Windows SCM（`sc create` + failure actions）、BusyBox inittab（`::respawn:`）、procd（`/etc/init.d/svcd`）。
- 安装需 root / Administrator；幂等（原子替换 + EnsureLine；`sc create` 1073 视为已存在）。
- `svcd --install [--prefix] [--conf <file>] [--no-service] [--target]`：复制/硬链 `svcd`、`svc`、`plugin`、`mcp` 到平台默认目录（Unix `/usr/sbin`，Windows `%ProgramFiles%\svcd`），再调用 `install-service --program <安装路径>`；`--uninstall` 逆操作，默认保留 config/data/logs，`--purge` 才清。除 `--manual` 外，安装会把安装目录写进**机器级 PATH**——Windows 追加到 `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment` 的 `Path`（写后广播 `WM_SETTINGCHANGE`），Linux 写 `/etc/profile.d/svcd.sh`，macOS 写 `/etc/paths.d/svcd`；重复安装不重复追加，`--uninstall` 删掉这一条。`--manual` 的启动脚本自带 PATH，不动机器环境（#311）。
- `svcd --install --conf <file>`：校验并原子复制到 `<config>/svcd.conf`。键：`bind`（默认 `127.0.0.1:8787`；非 loopback 需要 `auth_passwd`，缺失时降级 loopback；`SVCD_BIND` 覆盖）、`allow_insecure_install`（默认 false；开启后验签失败时降级接受未签名包，没有逐次开关）、`vendor_login`（默认 false；开启后同主机的厂商后台已登录时可用 `POST /vendor-login` 换控制台会话，见 ADR-0032）、`config_dir` / `data_dir` / `logs_dir` 三个根、`install_dir` 安装前缀（未显式写三个根时提供 `<dir>/config`、`<dir>/data`、`<dir>/logs`）、`auth_passwd`。相对路径相对配置文件所在目录；未知键/坏值拒绝且不覆盖已安装配置；已删除的 `plugins_dir` / `services_dir` / `temp_dir` 告警并忽略。`--uninstall` 默认保留，`--purge` 删除。
- 服务启动配置优先级：`--conf`（`--install` 把安装后的 `<config>/svcd.conf` 记录到服务命令行）→ 程序目录 `config/svcd.conf` → 程序目录 `svcd.conf` → `<config>/svcd.conf`（legacy）→ 生成默认（`<config>/svcd.conf`；不可写时程序目录）。生成的默认写全三个根的绝对值，派生目录只进注释；缺根的旧配置补齐一次并写回，本次启动立即使用补齐后的文档。`auth_passwd` 由 `svc lan passwd` 写入。每个键都可用 `SVCD_<KEY>` 覆盖（键名大写），环境变量优先于文件；空值视为未设置。模板见根目录 `svcd.conf.example`。
- 卸载默认保留 `<data>` 与配置，`--purge` 才删。

### 1.6 core 服务与 plugin 的边界

**core 服务是「直接声明的启动脚本或可执行文件」；plugin 是「标准 .spk 包」。两者是不同来源，不是同一包格式的两种模式。** 执行路径是共用的：plugin 可以在 manifest 里声明与 core 同形的 `lifecycle`，由同一个监督器跑 `start/stop/status`；区别仍在来源、信任与安装/配置 verbs，而不是「谁能有生命周期」。

| | core 服务 | plugin |
| --- | --- | --- |
| 来源 | 管理员在 `<config>/services.d/*.json` 或 `services.d/<id>.sh|bat|cmd|ps1` 声明的脚本 / 可执行文件；`svc` 是内置 core | `.spk` 包，经 `svc install` |
| 信任 | 无包签名；root 所有的定义文件 + 本机管理员 | Ed25519 manifest 签名 + `files[]` + `trust.d` |
| 安装/升级 | 由 OS 包管理器 / 管理员管文件；svcd 不安装、不升级 | `svc install`（升级同）/ `uninstall` |
| 配置 | 脚本/bin 自己管；svcd 不提供 `config` / `editconfig` / `export` | `config` / `configCheck` / `editconfig` / `.spkb` |
| 生命周期 | `start/started/stop/status`（JSON 定义）；drop-in 脚本仍是 `start/stop/status` | manifest 可声明同形 `lifecycle`；执行路径共用 |
| hooks | 无 | post-install / pre-remove |
| UI | 无 `ui`、无能力面 | `ui` + `/plugin-ui/{id}` |
| 注册表 | 启动时扫 `services.d/*.json` + 内置 `svc` | 扫 `<data>/plugins/*/current` |

`services.d/<id>.json`：

```json
{
  "schema": 1,
  "id": "my-core",
  "version": "1.0.0",
  "desc": "…",
  "protocol": 1,
  "exec": { "path": "/usr/local/bin/my-core", "args": [], "env": {}, "ready": "notify" },
  "dependencies": [],
  "lifecycle": {
    "start":   { "path": "/usr/local/bin/my-core", "args": ["prepare"], "timeout": 30 },
    "started": { "path": "/usr/local/bin/my-core", "args": ["started"], "timeout": 15 },
    "stop":    { "path": "/usr/local/bin/my-core", "args": ["stop"],   "timeout": 10 },
    "status":  { "path": "/usr/local/bin/my-core", "args": ["status"], "timeout": 5 }
  }
}
```

- `exec.path` 必须是绝对路径；Unix 可以是 shebang 脚本，Windows 是 `.exe` / `.cmd` / `.bat`；`ready` 同 plugin（默认 `notify`，脚本不讲话就用 `exec`；`port` 需要 `exec.port`，可加 `exec.probe_addr`，探测地址必须是 IP）。
- `exec` 是被监督进程。`lifecycle` 全部可选：`start` 是**前置准备**——每次 spawn `exec` 之前运行、必须退出，非零或超时即本次启动失败且 `exec` 不 spawn，崩溃重试会重跑（幂等）；`started` 是**就绪之后、Running 之前**的动作——该次 spawn 达到 Ready 后运行一次、必须退出，非零或超时即本次启动失败（停整组、跑 `stop`、进退避重试），每次 spawn（`svc start`、`svc restart`、崩溃重试）都重跑，所以同样必须幂等，跑的过程中对外仍是 `starting`、`svc start` 尚未返回，声明了就不得配 `remain_after_exit`（一次性服务没有可门控的就绪实例）；`stop` 是**收尾**——被监督进程消失之后运行一次（主动停、崩溃不重启、崩溃重启前、启动失败），跑的时候进程已经不在了，不要假设还能对它发信号；非零/超时只记日志。宿主负责终止（Unix 进程组 + 停止时快照到的 `setsid` 子孙 `SIGTERM` → 10 秒 → `SIGKILL`；Windows 直接强杀），`Stopped` 以 `stop` 返回为准（#323）。`status` 由 `svc status` 按需探测，stdout 作为 `details.probe` 的细节补充，退出码不改变服务状态。钩子 `path` 必须绝对，环境继承 daemon 并附加 `SVCD_SERVICE_ID` / `SVCD_ACTION` / `SVCD_DATA_DIR` / `SVCD_CONFIG_DIR` / `SVCD_LOG_DIR`；插件包（`kind=plugin`）的 `exec` 进程与 `lifecycle` 钩子只拿 `SVCD_PLUGIN_DIR`（安装目录 `<根>/<id>`）/ `SVCD_LOG_DIR` / `SVCD_TEMP_DIR` —— 插件是第三方代码，不给守护进程的配置根（`svcd.conf` 的密码散列、`trust.d`）与数据根，动作仍靠参数传（插件钩子**不拿** `SVCD_ACTION`，那是 core 专属）。
- `stop` 钩子额外拿两个变量，用来按「为什么结束」分支，而不是为每个状态配一个回调：`SVCD_STOP_REASON` = `stop`（显式 stop / restart 的停止半程）/ `shutdown`（daemon 整体退出）/ `crash`（进程非正常结束，含启动期崩溃）/ `exit`（进程正常结束且不重启）/ `start-failed`（start 或 started 动作失败、spawn 失败，进程可能从未产生）；`SVCD_LAST_EXIT` = 退出码或 `none`，**只有 `crash` / `exit` 时才是退出码**（停止时进程还活着；启动失败时上一代的码不算这一代的）。
- 文件 root 所有、0640；启动时读取，改定义后重启 svcd 生效（v1 不做热加载）。
- 不接受 `.spk` 专属字段：`platform` / `files` / `hooks` / `config` / `ui` 出现即定义非法；其他未知字段忽略。
- `id` 不能是 `svc`（保留给内置 core），且不能与 plugin id 冲突；冲突时 plugin 安装拒绝、core 定义加载拒绝。
- `<config>/services.d/<id>.disabled` 存在时该 core 服务 `enabled=false`：开机不自启、崩溃后不按退避自动重启；`svc enable/disable` 维护这个 sidecar，显式 `svc start` 仍然可用。
- **core 不接受 `capabilities`**：core 是生命周期单元，不参与 `tools/call`；定义里出现该字段即拒绝加载。能力面只属于 plugin。
- `<config>` 指平台配置目录（Linux `/etc/svcd`、macOS `/Library/Application Support/svcd/config`、Windows `%ProgramData%\svcd\config`）；user mode 的非 root 进程改用用户目录（Windows `%APPDATA%\svcd`）。`svcd.conf` 的 `config_dir` 可覆盖，`SVCD_CONFIG_DIR` 再覆盖，供测试与非 root 开发使用。`svcd.conf`、`services.d` 和 `trust.d` 都在这个目录；控制 socket 与 `http.port` 在 `<data>/run`，上传暂存在 `<data>/temp`。
- `<logs>` 指平台日志目录（Linux `/var/log/svcd`、macOS `/Library/Logs/svcd`、Windows `%ProgramData%\svcd\logs`）；`SVCD_LOG_DIR` 可覆盖，供测试与非 root 开发使用。
- core 与 plugin 走同一监督器、同一 stdio NDJSON、同一依赖图、同一日志 / 状态；`svc list/status/start/stop/logs` 对两者一致；`install/uninstall/import/export/editconfig` 只对 plugin。

### 1.7 脚本服务（drop-in）

`<config>/services.d/` 里同时接受**不写 JSON 的脚本服务**：

- Unix：`<id>.sh`；Windows：`<id>.bat` / `<id>.cmd` / `<id>.ps1`。
- 文件名即服务 id（必须符合 `[a-z0-9][a-z0-9_-]{0,63}`）；`.disabled` sidecar 同样生效。
- 脚本第一个参数是 `start|stop|status`，同时注入 `SVCD_ACTION`；**每个动作都是可选的**，脚本声明哪个，svcd 才调用哪个。
- 声明方式：`.sh` 的 shell 函数（`start()` / `function start`）或 `case` 分支（`start)`）；`.ps1` 的 `function start`；`.bat` / `.cmd` 的 `:start` 标签（大小写不敏感）。注释里的同名不计。
- 未声明的动作由框架兜底：缺 `start` → `exec.args` 为空，svcd 从头执行脚本；缺 `stop` → 不跑 stop 钩子，直接进入普通停止序列（Unix SIGTERM；Windows 对没有停止钩子的 `exec` 脚本立即结束进程）；缺 `status` → `svc status` 不追加 probe。
- 声明的 `start` 就是被监督进程（写进 `exec.args`），必须前台运行（可申请资源后 `exec` 真正的进程），不得 daemonize；`stop` 在进程消失之后跑，负责收尾而不是终止；`status` 是**细节补充**：stdout 作为 `details.probe`（截断到 8 KiB、多行以 `; ` 连接），退出码只追加到文本、不改变服务状态。
- 脚本派生的后台进程留在同一进程组（Unix）/ job object（Windows），停止时一起终止；调用 `setsid` 逃出进程组的子孙在停止那一刻按父链快照，同样一起终止（ADR-0031）。树里任一进程存活时服务仍是 `Running`，整棵树退出才结束。
- svcd 把脚本合成为 core 服务：`exec.ready = Exec`，声明的 `start` 写进 `exec.args`，`lifecycle` 只为 `stop` / `status` 生成钩子；默认 `version = 1.0.0`，无 dependencies / capabilities / config。
- 同 id 的 JSON 定义优先；脚本跳过并告警。脚本必须非 symlink；Unix 还要求 exec 位；改脚本后重启 svcd 生效（v1 不做热加载）。
- `.ps1` 在 Windows 通过 `powershell -NoProfile -ExecutionPolicy Bypass -File <script> <action>` 调用。

```sh
#!/bin/sh
start()  { exec /usr/local/bin/real-service; }
stop()   { ...; }
status() { ...; }
action="${1:-$SVCD_ACTION}"
case "$action" in start|stop|status) "$action" ;; *) exit 64 ;; esac
```

上面的例子声明了全部三个动作；只写 `start` 时，`stop` 由框架终止进程、`status` 不产生 probe。`svc list/status/start/stop`、日志、退避、依赖图与 JSON core 相同；`svc status` 的 `details.probe` 来自 `status` 动作的 stdout。

### 1.8 脚本插件（core 脚本转 .spk）

core drop-in 脚本可以原样放进 `.spk` 变成 plugin：脚本不改，补 manifest。转换后执行路径与 §1.7 相同，但来源与信任换成签名包，并额外获得版本、热安装/卸载、config/export 与 Web UI。

- manifest 用 `schema: 1`，`exec.ready: "exec"`，脚本声明的 `start` 写进 `exec.args`，`stop` / `status` 写进 `lifecycle`（路径是包内相对路径，且必须在 `files[]`）。
- payload 不是 native 可执行文件（ELF / Mach-O / PE）时可以声明 `interpreter`：`{name, args?, version?}`。`name` 是 PATH 上的裸名；`version` 是数字约束，**为空/省略只校验存在**，不执行版本探测。Windows 上只解析 `.exe` / `.com`，不接受 `.cmd` / `.bat` shim。声明了就一定要有脚本可跑（否则拒装）。
- 不声明也能跑：带 shebang 的脚本在 Unix 直接 exec，`.cmd` / `.bat` 在 Windows 经 `COMSPEC`；要锁定解释器版本或跑 `.py` / `.ps1` 时才声明。
- 安装时解析解释器到绝对路径并校验版本，失败不落盘；扫描时只重新解析路径。运行时统一 `interp + args + [script] + action_args`，`exec` 与 lifecycle 共用同一结果。Windows 没有 SIGTERM 的 polite 路径：进程被直接强杀，之后 `stop` 照常运行。
- 脚本插件通常 `capabilities: []`，MCP 面只剩 `<id>.start`；会讲 MCP 的脚本可以 `ready: "notify"` 并声明能力。只有一个常驻进程、只偶尔被调一下的插件（`ready: "exec"` / `"port"`）把能力挂成一次性钩子（`capabilities[].hook`），不必为了通道再养一个 shell。
- `plugin bundle --from-core --interpreter <name>` 会把声明的 `start` 写进 `exec.args`，为 `stop` / `status` 生成 lifecycle；lifecycle 指向的其它脚本也要一起进 `files[]`。`hooks` / `config` / `ui` 仍按插件规则。
- 没有版本门禁：`lifecycle` / `interpreter` / `compat` 与普通字段一样只在 `schema: 1` 内校验。

## 2. .spk 包格式

**.spk 只承载 plugin；core 服务不是 .spk（见 §1.6）。**

### 2.1 容器

- `.spk` = **ustar 子集，不压缩**；压缩交给传输层（store/CDN 的 `Content-Encoding`）。
- 条目顺序：`manifest.json` 第一、`signature.json` 第二，其余按路径排序；`mtime=0`、`uid/gid=0`、只允许普通文件与目录，可复现。
- 解包器拒绝：绝对路径、`..`、反斜杠、符号/硬链接、设备/稀疏文件、重复路径、清单外条目、setuid/setgid；总解压体积上限 512 MiB。
- 不用 zip（要 seek）、不用 tar.zst（核心里加解码器）。

### 2.2 manifest

JSON，`schema: 1`（唯一版本；旧线的 1/2/3 分级已合并，见 §1.8 与 §13 变更记录）。字段：

| 字段 | 类型 | 必需 | 语义 |
| --- | --- | --- | --- |
| `schema` | int | 是 | 只能是 `1`；其它值一律拒绝（旧线的包同时缺 `id`，不会被误接受） |
| `id` | string | 是 | 插件身份：`[a-z0-9][a-z0-9_-]{0,63}`，不能是 `svc`；目录名、日志名、`/plugin-ui/` 段、`<id>.<cap>` 工具前缀都用它 |
| `name` | string | 是 | 显示名：任意 UTF-8（可中文），≤128 字符，无控制字符；缺省等于 `id`；只给人看，命令与 API 不按它解析 |
| `version` | string | 是 | semver |
| `desc` | string | 是 | 一句话 |
| `platform` | string | 是 | 规范目标三元组，如 `x86_64-unknown-linux-musl`；设备与 `TARGET` 字符串精确相等，安装 + 加载双检 |
| `protocol` | int | 是 | 1 / 2 / 3；1 = 行分帧 JSON-RPC，2 = 长度前缀 JSON-RPC，3 = 多路复用帧（stream 0 是 JSON-RPC，stream 1..=0xFFFF_FFFE 是 raw 数据流，stream 0xFFFF_FFFF 是控制流）。不匹配安装拒绝、加载再检 |
| `exec` | object | 是 | `{path, args?, env?, ready?, port?, probe_addr?}`；`ready` 默认 `notify`，可 `exec` 或 `port`；`port` 只在 `ready="port"` 时用，`probe_addr` 是探测地址（默认 `127.0.0.1`，必须是 IP） |
| `interpreter` | object | 否 | `{name, args?, version?}`；声明了就必须有脚本 payload，不声明则直接 exec（见 §1.8） |
| `lifecycle` | object | 否 | `{start?, started?, stop?, status?}`；`start` 是前置准备（必须退出），`started` 在就绪后、Running 之前跑一次（失败即启动失败），`stop` 在进程消失后运行并拿 `SVCD_STOP_REASON`；路径包内相对且在 `files[]` |
| `dependencies` | [string] | 否 | 服务 id 列表，**无版本约束** |
| `capabilities` | array | 否 | `{name, desc, inputSchema, outputSchema?, hook?}`；name 插件内唯一，`svc.` 禁用；一个能力只有一种投递方式——`ready=notify` 走通道且不许声明 `hook`，`ready=exec`/`port` 没有通道，因此**每个**能力都必须带 `hook`（`{path, args?, env?, timeout?}`，包内相对且在 `files[]`）：每次 `tools/call` 起一次该动作，参数写 stdin，stdout 原文成为这次调用的文本结果（不做 JSON 校验，与通道那侧 handler 的 stdout 同形；空输出就是空文本），stderr 进 `<id>-hooks.log`，超过契约内联上限的结果算这次调用失败而不是截断；`outputSchema` 只做声明与 MCP 投影，不校验结果（省略 = 未声明，`{}` = 任意） |
| `ui` | object | 否 | `{type: web / gui / headerless, entry?}`；`entry` 相对 `payload/web/`，默认 `index.html` |
| `icons` | object | 否 | `{png?, ico?, icns?}`，值必须是 `files[]` 里的包内路径；每项 ≤1 MiB、合计 ≤2 MiB、按魔数校验；至少一项；未知键拒绝 |
| `files` | array | 是 | 除两个清单外的全部文件：`{path, size, sha256, mode}` |
| `hooks` | object | 否 | `{postInstall?, preRemove?}`，指向 `hooks/` 内可执行 |
| `config` | string | 否 | 配置模板在包内的相对路径，默认 `config/config`；必须在 `files[]` |
| `configCheck` | object | 否 | `{path, args?}`；校验命令，path 在 `files[]`、可执行；声明 check 却没有 config → 安装失败 |

- 文件名是 `<id>-<version>-<platform>.spk`，带**完整目标三元组**（只写 arch 无法区分 musl 与 gnu）；声明了 `compat.devices` 的包再追加列表**第一个** id（`<id>-<version>-<platform>-<device>.spk`），这个后缀只是人读提示，完整列表仍以 manifest 为准；`platform` 字段本身仍不带 `arch` 字段（见 §11）。
- **canonical manifest**：打包器必须产出「排序键 + 紧凑 + 无尾换行 + 无 BOM」的 `manifest.json`；签名覆盖原样字节。
- 未知字段忽略；未知 `schema` 主版本拒绝；主版本内加法兼容。
- `arch` 不是合法字段；v1 不提供 `arch` 兼容层（见 §11）。

### 2.3 签名与信任

- 包内 `signature.json`：`{"alg":"ed25519","keyId":"...","sig":"base64"}`，签 `manifest.json` 的原样字节；`files[]` 钉每个文件的 sha256/size/mode。
- 验证顺序：读 manifest → 验签 → 校验 schema / platform / 路径 → 逐文件验 hash 后落盘；清单外条目直接拒；流式、逐文件可验。
- `plugin verify <file.spk> [--json]` 只做包自身校验（清单 / 文件哈希 / 签名结构；不要求平台匹配、不要求 key 在 trust.d、不解析解释器），不落盘。`--json` 输出 `{ok, file, id, name, displayName, version, platform, schema, size, sha256, capabilities, compat, signature:{state,keyId,trusted}, host:{matchesHost,device,interpreter}, files}`；`ok` 只表示包自身完整，`signature.trusted` 与 `host.*` 是交给调用方判断的事实。`install` 仍走 `plugin::verify` 的严格策略，二者不共用“是否可安装”的结论。
- 信任根：`<config>/trust.d/<keyId>.pub`（**PEM SPKI**）；未知 keyId 默认拒绝。未签名安装只由 `svcd.conf` 的 `allow_insecure_install` 控制：开启后安装路径先严格验签，失败才降级接受缺 `signature.json` 或未信任 key 的包，并在日志中标注未验签；CLI/HTTP/Web 都没有逐次 insecure 参数或勾选项。
- 信任根由管理员管理：`svc trust list|add <file.pub>|remove <keyId>`，或 Web 控制台的 `POST /api/v1/trust?key_id=<id>` 上传；`svcd --install` 会写入内置的 store 发布钥，同名文件已存在时不覆盖。包内永不携带自己的信任根（自签自证没有意义）。
- `svcd --install` 把内置的 store 发布公钥写到 `<config>/trust.d/signing.pub`；已存在的同名文件不覆盖，便于管理员轮换。`SVCD_TRUST_DIR` 可覆盖 trust.d，供测试与非 root 开发。
- 不签整个 tar、不做分离 `.sig`、不只靠商城 TLS。

### 2.4 安装布局与策略

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| data | `/var/lib/svcd` | `/Library/Application Support/svcd` | `%ProgramData%\svcd` |
| config | `/etc/svcd` | `/Library/Application Support/svcd/config` | `%ProgramData%\svcd\config` |
| logs | `/var/log/svcd` | `/Library/Logs/svcd` | `%ProgramData%\svcd\logs` |

- 版本目录 `<data>/plugins/<id>/<version>/` 保留包内布局；`<id>/current` 是版本号文本、`<id>/enabled` 是标记。
- **keep-old**（显式 `--strategy keep-old`）：新版本目录并存 → fsync → rename → 切 `current`；运行中的进程不热切，下次 start 用新版本；保留一份旧版本，`prune` 清非 current 且非运行中的版本。
- **replace**（默认，所有安装入口）：验签 + 校验清单（不落盘）→ 停服务 → 检查 plugin 目录空间：`free >= new` 则把旧版原地改名为 `.backup-<version>.<pid>` → 解包 → 写 `current` → 复制 config 模板；成功删备份，失败还原旧版与 `current`；`free < new` 但 `free + old >= new` 则删旧后装新（失败无回滚）；都不够则拒绝。空间按解包后体积计算。
- 空间不足自动降级为 `replace`；`install -` 只在 `replace` 下允许；降级需 `--allow-downgrade`；运行中的版本永不删；卸载默认保留 config/state，`--purge` 才清（并删除服务/安装钩子日志）。
- 所有安装入口默认 `replace`；`--strategy keep-old` 显式保留。Web 控制台不提供策略选择。

### 2.5 hooks

- 只接受 `hooks/` 下的可执行文件（Unix exec 位 + shebang，安装验包时校验；Windows `.exe` / `.cmd` / `.bat`，在执行 hook 时校验）；不解析内联命令。
- 环境清空后只给 `PATH`、`SVCD_PLUGIN_ID`、`SVCD_PLUGIN_VERSION`、`SVCD_PLUGIN_DIR`（版本目录的父目录，即插件目录）、`SVCD_ACTION`、`SVCD_DATA_DIR`、`SVCD_CONFIG_DIR`；Windows 额外给 `SystemRoot` / `WINDIR`（启动 `cmd.exe` 必需）；cwd = 版本目录；stdout/stderr 进 `<logs>/<id>-hooks.log`；60s 超时杀进程组。
- post-install 失败 → 报错；是否回滚由安装策略决定：replace 走既有回滚，keep-old 保留新旧两个版本（current 已切到新版本）。pre-remove 失败 → 记日志但继续卸载。
- 嵌入式构建（`--no-default-features`）默认拒绝带 hooks 的包，`plugin install --allow-hooks` / HTTP `allow_hooks=true` 放行；桌面构建（默认 features）默认放行。
- 多根安装（ADR-0030）：`plugin install --root <dir>` 与 HTTP `?root=` 都是"必须出现在 `plugins_dir` 里"的偏好；不给就按空闲字节升序挑第一个装得下的根。已装插件只在它被记录的根上升级，换根要先 `uninstall`（配置默认保留）。
- 桌面交互安装应先列出 hooks 再确认（`--yes` 跳过）：**尚未实现**——当前 CLI/HTTP 安装默认放行且没有交互确认步骤。
- **hooks 权限 = 安装者权限（root）**；信任只来自签名。
- **实现状态**：hook runner 已实现（`src/hook.rs`）：60s 超时、环境清空、Unix 进程组 / Windows 每 hook job 对象杀进程树、`<id>-hooks.log`；post-install 失败只返回错误，回滚完全由 keep-old / replace 策略处理；pre-remove 失败只记日志。

### 2.6 config（详见 §5.3）

- 模板在签名范围内（`files[]`），活动副本不在；活动副本在 `<data>/plugins/<id>/config`，root:svcd 0640。
- 安装时缺则复制模板、存在则不覆盖；升级保留；模板 hash 变化写 `config.new`；卸载默认保留。

## 3. IPC、MCP 与 HTTP UI

### 3.1 控制通道

- 一套 **JSON-RPC 2.0 + NDJSON** 信封跑三条链路：插件 stdio、`svc`/`mcp` 的 UDS / 命名管道、UI 的 `POST /plugin-ui/{id}/{token}/rpc`。这三条是控制面；HTTP 的 `/mcp` 与 `/api/v1` 是另外两个面（§3.4 / §3.5），不复用 NDJSON 分帧。
- `svc`/`svcd` 的本地管理通道用 UDS（Unix）/命名管道（Windows）上的 NDJSON，按权限分**两个端点**：`svcd`（载读 + 生命周期：`status` / `start` / `stop` / `restart` / `logs` / `export` / `editconfig --print`）与 `svcd-admin`（载写：`install` / `upgrade` / `uninstall` / `import` / `editconfig` / `enable` / `disable` / `auth`）。**鉴权就是端点 ACL，请求不携带凭据**：Unix 侧 `svcd` socket 0660（root:svcd）、`svcd-admin` 0600；Windows 侧两条显式 protected DACL（C8）。user mode 也有两个端点，但两者共用同一条 ACL（当前用户私有）：方法分档照旧，客户端不需要知道自己在哪种模式。旧版本遗留的 `<config>/svcd.token` 由启动/安装路径清理。
- 帧：UTF-8、LF、禁内嵌换行与 BOM；Windows 二进制模式；单帧上限 1 MiB，超限 = 协议违规，按崩溃路径杀进程。
- 流控：每插件在途 64、排队 128；超出 `-32000 busy`；单请求默认 30s，超时先发 `notifications/cancelled` 再回错；id 用 svcd 侧单调 u64。
- 插件 → svcd 只允许响应与通知，不允许发请求（v1 不做双向 RPC）；stdout 只走协议帧，stderr 是行式日志；stdout EOF = 退出。
- 方法集 v1：`svcd.ping`（必需，就绪探针）、`tools/call`（必需）、`svcd.status`（可选）、`svcd.shutdown`（可选通知；**BusyBox 上不提供**，见 §13-C7）、`notifications/cancelled`、`notifications/message`。
- `tools/list` 不进路由：能力来自 manifest，运行时自报会与注册表分叉。
- 版本：manifest `protocol: 1`；不匹配安装拒绝、加载再检；无握手；每条消息可带 `_meta` 做诊断，未知 `_meta` 忽略。
- Windows 稳定版无 UDS，桌面走命名管道；命名管道必须显式设 DACL（不要用默认 Everyone 读）。
- **实现状态（2026-11-12，#31）**：插件 stdio 的 NDJSON 分帧（1 MiB）、svcd 侧单调 id、每插件 64 在途、30s 超时先发 `notifications/cancelled` 再回工具错误、收帧超限杀进程、stdout EOF 按退出处理、插件 stdout 非协议帧按违规处理、崩溃摘能力并失败在途调用均已实现；svcd 暴露 `registry.tools` / `registry.call`（别名 `tools/list` / `tools/call`），能力描述与 `inputSchema` 从 manifest 透传。**本票未实现 §3.1 的 128 排队**：第 65 个并发调用直接回 `-32000 busy`（票面验收范围），排队留给后续票。

### 3.2 MCP 聚合

- 语义在 svcd，协议在 `mcp` 桥：svcd 提供 `registry.tools` / `registry.call` / `svc.*`，不认识 `_meta` / `resultType` / `ttlMs`；桥负责 era 判定、`_meta` 校验、缓存装饰、错误透传。
- 每个客户端一个桥进程；桥不拉起 svcd，连不上 stderr 报错 + exit 1；多个桥并发连同一个 svcd。
- dual-era：首个请求是 `initialize` → legacy；否则 modern（按 `_meta` 校验）。modern `2026-07-28`；legacy 支持 `2025-11-25` / `2025-06-18` / `2025-03-26`。
- modern：`server/discover`（MUST）；每请求校验 `_meta.protocolVersion` + `clientCapabilities`；result 带 `resultType:"complete"`；`tools/list` 带 `ttlMs:1000`、`cacheScope:"private"`，`listChanged:false`。
- legacy：本地回 `initialize`；`capabilities.tools.listChanged:true`；桥每 1s 轮询 `registry.version`，变了发 `notifications/tools/list_changed`。
- 错误映射：未知工具 / 参数不合法 / 缺 `_meta` → JSON-RPC error；已知但当前不可调用 → `isError:true` + 先 `<id>.start`；崩溃/超时/插件 error → `isError:true`；协议违规 → `isError:true` 并按崩溃处理；桥连不上 svcd → 在途调用 `isError:true` 后 exit 1。
- `inputSchema` 原样透传，聚合层**不做 JSON Schema 校验**（显式偏离，插件自己校验）；`svc.*` schema 由聚合层写死。
- 工具名 `<serviceId>.<cap>`，按名稳定排序；死 plugin 只出 `<id>.start`。
- `tools/list` / `tools/call` 只聚合 plugin 能力；core 不出现在工具面。`svc.*` 是管理工具，不是 core 的能力。
- **实现状态（2026-11-12，#32）**：`mcp` stdio 桥已实现 dual-era：首个请求 `initialize` → legacy，否则 modern；modern `server/discover`、每请求 `_meta` 校验（缺字段 `-32602`，版本不符 `-32022` + `data.supported`）、所有成功 result 带 `resultType:"complete"`、`tools/list` 带 `ttlMs:1000` / `cacheScope:"private"`、结果附 `_meta.serverInfo`；legacy `initialize` / `ping` / `notifications/initialized`、1s 轮询 `registry.version`（工具列表的稳定哈希）并在变化时发 `notifications/tools/list_changed`。桥不拉起 svcd，连不上 stderr + exit 1；每个客户端一个桥进程，多个桥并发连同一个 svcd。

### 3.3 HTTP UI

- `/plugin-ui/{id}/{token}/…` 映射包内 `payload/web/`；`/plugin-ui/{id}/{token}/rpc` 是 POST JSON-RPC，转给该插件的 stdio 通道——同一信封，没有第二套协议。
- `{id}` 必须是合法路径段，`{token}` 必须绑定该 id；SPA fallback 只对 `Accept: text/html`；HTML 注入 `<base href="/plugin-ui/{id}/{token}/">`，资源必须相对路径；v1 不做响应体重写。
- MIME 按扩展名；ETag = mtime+size；`no-cache` + 304；v1 不做 Range。
- 实时通道（2026-11-13 起，#73）：
  - `GET /plugin-ui/{id}/{token}/ws`：RFC 6455 WebSocket，只收文本帧；客户端发 `tools/call`（与 `/rpc` 同一语义），插件 `notifications/*` 广播回同一连接；1 MiB 消息上限、ping/pong/close、无压缩/子协议。
  - `GET /plugin-ui/{id}/{token}/events`：SSE，`text/event-stream`；插件 `notifications/*` 以 `event: message` / `data: <json>` / `id: <seq>` 下发，15s 心跳；上行仍走 `POST /rpc`——SSE 单向 + POST 合起来才是双向。
  - 事件源是插件 stdout 的 `notifications/*`（`notifications/message` 为主）；按插件广播，每插件最多 64 个订阅者、有界队列，慢客户端断开，不阻塞插件 stdout。
  - 每插件保留 256 条 / 2 MiB 的事件环；SSE 重连带 `Last-Event-ID` 时先补发 `seq > last` 的事件，再进入实时流。请求的 id 已早于环窗口时先发 `event: reset` + `data: {"reason":"replay-unavailable"}`，UI 应调 `tools/call` 重新拉状态；无/非法 `Last-Event-ID` 只进实时流。
  - 两条流复用 `/plugin-ui` 鉴权、per-plugin token 与同源 `Origin` 检查：LAN 未登录 401、token 不匹配 401、跨源 403；WS 的 cookie 调用必须带 `Origin`。
- `bind` 默认 loopback；LAN 需在 `svcd.conf` 显式改 `bind`。**插件不自起 HTTP**；外连外部 WS/SSE 仍由插件进程自己负责，核心只做 UI 侧通道。
- **实现状态（2026-11-15）**：SSE/WS 两条 UI 流仍按插件广播；URL 为 `/plugin-ui/{id}/{token}/events` 与 `/plugin-ui/{id}/{token}/ws`，token 绑定 plugin id。`GET /plugin-ui/{id}/{token}/events` 走 chunked `text/event-stream`，事件来自 `Slot::handle_notification` 发布的进程内 EventBus；每插件保留 256 条 / 2 MiB 事件环，`Last-Event-ID` 重连先补发缺失事件，越界先发 `event: reset`。`GET /plugin-ui/{id}/{token}/ws` 完成 RFC 6455 握手/掩码/帧编解码，客户端文本帧只接受 `tools/call`，响应与插件 `notifications/*` 走同一连接；1 MiB 上限、ping/pong/close、64 订阅者/插件。两条流复用 session/Bearer、per-plugin token 与同源 Origin；WS cookie 必须带 Origin。
- **实现状态（2026-11-15）**：`GET /plugin-ui/{id}/{token}/…` 从 `<data>/plugins/<id>/<version>/payload/web/` 提供静态资源（MIME 按扩展名、`Cache-Control: no-cache`、ETag=mtime+size、`If-None-Match` → 304、`X-Content-Type-Options: nosniff`、`Referrer-Policy: no-referrer`）；HTML 在 doctype 后注入 `<base href="/plugin-ui/{id}/{token}/">`；SPA fallback 只对 `Accept: text/html`，缺失的脚本/样式仍是 404；`{id}` 必须通过 `package::valid_name`，`{token}` 必须绑定该 id，路径逐段解码后拒绝 `.`/`..`/反斜杠/NUL，并对 canonicalize 后的路径做根目录越界检查。`manifest.ui.type` 只接受 `web`/`gui`（`headerless` 或无 `ui` 即 404）。`POST /plugin-ui/{id}/{token}/rpc` 只桥 `tools/call`：`params.name` 带不带 `<id>.` 前缀均可，转发为 `registry.call` 并把页面 id 回显；`-32700/-32600/-32601/-32602` 在本地返回，插件未运行是 `result.isError:true`（同一信封）。LAN 开启时 `/plugin-ui/*` 要求会话；token 不对或缺失 401。`/rpc` 校验同源 `Origin`，cookie 调用必须带 `Origin`，但不额外要求 `X-Svcd-Csrf`（同源 Origin + SameSite=Strict 承担 CSRF/DNS rebinding）；`/plugin-ui/{id}/{token}`（无斜杠）在鉴权后 308 到带斜杠形式。v1 不做 Range；WebSocket/SSE 见上方实现状态。

### 3.4 HTTP MCP 入口

- `POST /mcp`：MCP **modern-only**（2026-07-28）Streamable HTTP；JSON 响应，**无 session、无 SSE**；`server/discover`、`tools/list`、`tools/call` 与 stdio 桥共用 `registry.tools` / `registry.call` 语义，不另造工具面。
- 按 2026-07-28 Streamable HTTP 的传输要求校验 `MCP-Protocol-Version` 等头；正文每请求校验 `_meta.protocolVersion` + `clientCapabilities`；result 带 `resultType:"complete"`；`tools/list` 带 `ttlMs:1000` / `cacheScope:"private"`；不做 `subscriptions/listen`。
- 认证：`Authorization: Bearer <password>`；不接受 cookie（避免 CSRF 面）；401 不带密码，403 密码错。
- 监听：与 HTTP UI 同一个 listener；默认 loopback，LAN 需显式开启；单请求体上限 1 MiB。
- legacy 客户端走 `mcp` stdio 桥（§3.2）；HTTP 只做 modern。
- 工具可见性/错误映射与 §3.2 完全相同；死服务只出 `<id>.start`。
- **实现状态（2026-11-12，#40）**：`POST /mcp` 已实现 modern-only Streamable HTTP：单请求单 JSON 响应、无 session / 无 SSE，`GET`/`DELETE` 返回 405。逐请求校验 `MCP-Protocol-Version`（必须等于正文 `_meta` 版本）、`Mcp-Method`、`tools/call` 的 `Mcp-Name`（支持 `=?base64?…?=` 哨兵）：头缺失或头/正文值不一致返回 400 + `-32020`；正文缺 `_meta` 字段按核心规范 400 + `-32602`（与 stdio 同一条错误）；未知方法 404 + `-32601`；未实现版本 400 + `-32022`（带 `supported`）；`Origin` 必须同源（否则 403）。`Authorization: Bearer <password>` 必需：缺密码 401（带 `WWW-Authenticate: Bearer`）、密码错 403，cookie 完全不参与；未设置 `auth_passwd` 时端点保持关闭，`svc lan passwd <password>` 可在不开 LAN 时设置密码。`server/discover` / `tools/list` / `tools/call` 走与 stdio 桥相同的 `mcp::serve_modern`（`resultType:"complete"`、`ttlMs:1000`、`cacheScope:"private"`、`<id>.start` 可见性、`isError` 映射）；正文上限 1 MiB（超限 413）；通知 POST 返回 202 空体（该修订未定义通知的头要求，故不做镜像校验）；控制通道不可达仍回 JSON-RPC（与 stdio 同映射，不引入 503）；不做 `subscriptions/listen`。

### 3.5 HTTP 管理 API

- 前缀 `/api/v1`，JSON only（上传/下载为二进制），无 CORS；**覆盖 `svc` 的全部管理命令**：
  - `GET /api/v1/services` → `{services:[summary + hasConfig + hasUI + icon]}`（§5.2 的 summary；`hasConfig` / `hasUI` 是 HTTP 列表附加项，表示活动配置与可打开 Web UI 是否存在；`icon` 是包声明图标的地址，没有图标时缺省）
  - `GET /api/v1/services/{id}` → `{summary, details}`
  - `POST /api/v1/services/{id}/start` → 同步到 ready，返回 `{summary}`；超时 504
  - `POST /api/v1/services/{id}/stop` → 返回 `{summary}`
  - `POST /api/v1/services/{id}/restart` → 等价 `svc restart`（组终止 → stop 动作 → start 动作 → spawn → started 动作），同步返回 `{summary}`；插件页面改完启动期配置后用它重跑
  - `POST /api/v1/services/{id}/enable` / `disable` → 切换开机自启，返回 `{summary}`
  - `GET /api/v1/version` → `{version, protocol, platform}`
  - `GET /api/v1/disk[?id=<插件 id>]` → `{roots:[{root,freeBytes,default,plugin,volume}], tempFreeBytes, tempVolume}`：`roots` 是**列表**，每个插件根一行，按优先级排列（`default=true` 是默认安装根）；根目录不存在就当**量不到**、给 `null`（不向上找祖先：那会把父目录所在文件系统的容量算成未挂载卷的）；拿不到数字的卷同样给 `null`。`?id=` 让每行多一个 `plugin` 字段（`{id,version,directory,bytes}`，没装为 `null`；`bytes` 是当前版本目录的实际占用，升级判断要用它，量不到给 `null`），调用方据此区分「原地升级」（这个根已经有它）和「新装」（挑一个装得下的根）；id 不合法 400，未知 id 不是错误。多根配置下每行就是一个候选根（ADR-0030）。`tempFreeBytes` 是 `<data>/temp` 的空闲字节，装包要经过它；`volume` / `tempVolume` 是所在卷的不透明标识（同一文件系统相等，量不到为 `null`），装前预检用它判断「下载目录与 temp 是不是同一块卷」——同卷是一次 rename，跨卷是整包复制，占两份空间
  - `GET /` → 内嵌 Web 控制台（静态 HTML；页面本身不需要认证，API 调用仍走同一鉴权）
  - `GET /assets/{*path}` → 控制台的其余静态文件（同样免认证；页面只在 `/`）
  - `POST /api/v1/plugins`：body 为 `.spk` 原始字节（`application/octet-stream`），query `strategy` / `enable` / `allow_hooks` / `root`；流式解包到**选中的插件根**（`root` 显式指定，否则装得下且剩余空间最小的根，ADR-0030），512 MiB 上限；返回 `{id,version,state}`。`root` 不在 `plugins_dir` 里 → 400 并列出可选根；同一 id 已装在别的根 → 409（先卸载）；都装不下 → **507 `insufficient_space`**，消息里带 `need` 与每个根的空闲字节，`error.data` 里同一组数字是机读的：`{id, need, installed, root, roots:[{root, free}]}`（`installed` 是当前版本目录的实际占用，`root` 是这次检查的那个根、自动选根时为 `null`，`free` 量不到为 `null`）。`need` 只算这次要写入的新树：旧版在提交前仍在盘上，但它的字节已经不在 `free` 里，再加一次就是要求放下两份（#329）。控制台据此弹「卸载旧版并安装」：卸载用 `purge=false` 保住配置与数据，再用同一个文件重传（`.spkb` 导入走同一套空间检查，也弹同一个框，重传的仍是同一个文件）；能不能回收只按 `root` 那个根算——卸载只往这个根还空间，别的根再空也帮不上（`root` 为空时退回取最大值，#330）
  - `POST /api/v1/plugins/{id}/ui-token`：签发 1 小时、绑定该 plugin id 的 UI token；返回 `{token,url,expiresIn}`，`url` 形如 `/plugin-ui/{id}/{token}/`
  - `GET /api/v1/plugins/{id}/icon`：返回当前版本包声明的图标（优先 svg，其次 png / ico；`icns` 不供浏览器）；没有图标或读不到时 404。列表项的 `icon` 字段就是这个地址
  - `GET /api/v1/trust` → `{keys:[keyId], directory}`；`POST /api/v1/trust?key_id=<id>`：body 为 PEM SPKI 公钥，写入 `<config>/trust.d/<id>.pub`；`DELETE /api/v1/trust/{keyId}` 撤销
  - `DELETE /api/v1/plugins/{id}?purge=false&force=false`：卸载；`purge=true` 删除版本、配置、状态与 `<id>.log` / `<id>-hooks.log`（含轮转），Web 控制台的「删除」用 `purge=true`；空间不足的恢复流程用 `purge=false`，让重装还能找回配置与数据。卸载成功后清掉该 id 的位置索引记录。插件所在的根不可用（SD 卡拔了）时返回 409 并带上该路径与 `--force` 提示；`force=true` 只删那条记录、不碰任何卷，并写一行审计日志（`location record forgotten … the volume was left untouched`）
  - `DELETE /api/v1/blobs/{id}` / `DELETE /api/v1/file/{id}`：立刻释放一个传输产物（单个 blob，或整包 `files/<id>` 的清单与分片），返回 `{removed:true}`；本来就不存在返回 404。商店在浏览器取完包字节后调用它，把一次安装的磁盘峰值压到**一份包**；不定时依赖 `temp_retention_secs`
  - `POST /api/v1/import?config_id=<id>&no_config=false&force=false`：body 为 `.spk` / `.spkb` / 裸配置，自动识别，语义同 CLI
  - `GET /api/v1/export/{id}/spk`：从已安装版本重建的可安装、已签名 `.spk`（`application/octet-stream`；Web 控制台“导出”使用它）。根不可用时 409 `root_unavailable`（带路径），不是 404——插件在，只是它的卷不在
  - `GET /api/v1/export/{id}`：整包 `.spkb` 备份容器（`backup.json` + `plugin.spk` + 配置；供备份/导入）；根不可用同样 409 `root_unavailable`
  - `GET /api/v1/export/{id}/config`：单配置
  - `GET /api/v1/config/{id}`：活动配置原始字节（`text/plain`；无配置 404）
  - `PUT /api/v1/config/{id}`：body 为候选配置；跑 `configCheck`，原子替换，保留 last-good；校验失败 409
  - `POST /api/v1/config/{id}/reset`：模板覆盖
  - `GET /api/v1/logs?lines=200`：daemon 自己的 `<logs>/svcd.log` 文本尾部（与插件日志同一套 `lines` / `level` 参数）；`lines` 上限 2000；不提供 follow / SSE
  - `GET /api/v1/logs/{id}?lines=200`：插件 `<id>.log` 的文本尾部；同一套参数
  - `DELETE /api/v1/logs` / `DELETE /api/v1/logs/{id}`：清空 daemon 日志或插件日志（`<id>.log` + `<id>-hooks.log` + 两者轮转）。清空走写侧 `LogSink::reset()`（关句柄 → 删文件 → 重开），不靠 HTTP 删文件：Windows 上删一个仍被写的文件会变成 delete-pending，写侧继续往看不见的旧文件追加。审计行 `logs cleared: <id> (N files)` 写进 daemon 日志；没有可清文件时 404
  - `GET /api/v1/upgrade/check`：`{store,current,latest,asset,sumsAsset,newer}`。`store=false` 表示没装 plugin-store（响应仍是 200，控制台据此提示两条出路）。
  - `POST /api/v1/upgrade/apply`：无 body。检查 → 下载归档与 `SHA256SUMS`（两次 `release_asset`，一次一个 blob）→ sha256 校验 → 用平台 `tar` 解包 → 替换 `svcd`/`svc`/`plugin`/`mcp` → 重启，走和设置保存、`svcd restart` 同一条路（#317）。失败返回 400 并带原因（没装 store、hash 不符、归档里没有二进制、开发树拒绝升级）。`svcd upgrade --file <归档> [--sum <SHA256SUMS>]` 是离线同一条链。
  - `GET /api/v1/settings`：`{path, keys:[{key,group,kind,default,doc,recommend,editable,needsRestart,source,value,effective}]}`。`source` 是 `file` / `env` / `derived`；`effective` 是**当前 daemon 实际在用的值**（派生数字来自 `limits`，路径来自目录集，密码只报 `true`/`false`）。
  - `PUT /api/v1/settings`：body `{values:{"<key>":"<value>"|null}}`，`null` 表示删除该键回到默认。只接受控制台可改的键；未知键、只读键（路径与 probe 输入）、`mem_budget_bytes`/`mem_budget_percent` 同时存在、值不能解析，都返回 400 且**不写盘**。写盘是一次「读-改-写」：只动点名的键，注释与其它手写行逐字节保留；`auth_passwd` 收明文、落盘前派生 pbkdf2 哈希。成功后按角色重启：user mode 返回 `{saved,restarting:true}` 并启动 detached helper（先让 HTTP 响应发出，再停旧进程、等控制通道释放、用同一份环境拉起新进程）；服务模式里有 init 系统（systemd / OpenWrt procd / BusyBox `::respawn:` / launchd）时返回 `{saved,restarting:true}`，把重启交给 `svcd service-restart-helper`；没有可用的重启程序（`--manual` 安装、Windows SCM）时返回 `{saved,restarting:false,hint}`，hint 是 `restart the svcd service yourself: <该平台的命令>`。升级与 `svcd restart` 用的是同一个判断与同一句文案（#317）。
  - `GET /api/v1/auth`：`{bind,requestedBind,authRequired,passwordSet}`
  - `POST /api/v1/auth/enable`（body `{bind}`）/ `POST /api/v1/auth/disable`
- `plugins` / `export` / `config` / `import` 端点只对 plugin；core 服务不能经 HTTP 安装、卸载、导出或编辑配置；`GET /services` 两者都返回。
- 认证：`Authorization: Bearer <password>`，或登录时下发的 `svcd_api` cookie（Path=`/api/v1/`）；cookie 的写请求必须带同源 `Origin` + `X-Svcd-Csrf: 1`。
- **登录密码等价于 root / Administrator**：install、uninstall、import、editconfig 都通过它授权。默认 LAN 关闭、无 TLS；只用于可信网络（见 §6 / §13-C5）。
- 语义映射 `svc.*`，与 CLI 同源；不写第二套状态。错误：`{error:{code,message}}`，400 / 401 / 403 / 404 / 409 / 413 / 500；start/stop 幂等。
- 上传大小：`.spk` / `.spkb` 512 MiB（与解包上限一致）；配置 1 MiB；日志响应 1 MiB。
- HTTP/1.1 keep-alive：同一连接顺序处理多个请求；`Connection: close` 显式关闭；不支持 pipelining；每个连接仍占一个线程。
- **嵌入式构建（#86）**：默认 feature `http`（HTTP 管理 API、内嵌控制台、插件 Web UI、LAN、MCP HTTP）。`cargo build --release --no-default-features` 移除这些，保留 daemon/supervisor、channel、CLI 与 MCP stdio；Windows 二进制约 1.14 MB → 1.00 MB，musl 约 1.36 MB → 1.21 MB。HTTP/channel/plugin reader/WS worker 线程使用 128–256 KB 显式栈（默认约 2 MB）；musl 下 daemon VM @30 插件由约 128 MB 降到约 11.6 MB，RSS 基本不变。
- **实现状态（2026-11-13，#79/#83）**：HTTP 监听改为阻塞 accept；daemon 改用 condvar 唤醒控制通道和插件 stdout，事件循环只在 50ms 兜底等待中检查 shutdown/定时器，无 1ms 轮询；`/api/v1` 与 `/mcp` 支持 HTTP/1.1 keep-alive（顺序多请求，`Connection: close` 可关闭），WS/SSE 仍独占连接。性能对比见 `docs/research/2026-11-13-core-compat-and-performance.md`。
- **实现状态（2026-11-12，#37 / #38 / #39）**：已实现 `GET /services`、`GET /services/{id}`、`POST start/stop`、`GET /version`、`POST /plugins`、`DELETE /plugins/{id}`；`bind` 默认 `127.0.0.1:8787`（`SVCD_BIND` 覆盖；`0` 端口写 `<data>/run/http.port`），非 loopback 需要密码 cookie/Bearer，**无 TLS**；`svc lan enable/disable` 已删除，认证由 `bind` 推导。#39 补齐 `GET/PUT /config/{id}`、`POST /config/{id}/reset`、`GET /export/{id}/spk`（可安装 `.spk`）、`GET /export/{id}`（`.spkb` 备份）、`GET /export/{id}/config`、`POST /import`（自动识别）、`GET /logs/{id}?lines=N`（默认 200、上限 2000、不提供 follow）、`POST /services/{id}/enable|disable`；这些端点只接受 plugin id，core 服务返回 404，方法不符返回 405。`GET /` 提供内嵌控制台（插件列表/安装/启停/自启/打开/配置/日志/导出）：上传按钮直接弹文件框，公钥上传在顶栏，无配置的插件不显示“配置”，有 web/gui UI 的插件显示“打开”，日志弹框可 3 秒自动刷新，删除走 `purge=true`。“安装包”导出 `/export/{id}/spk`（签名包，使用包内配置模板）；“导出备份”导出 `/export/{id}` 的 `.spkb`（签名包 + 当前活动配置），可直接在“上传安装”里选择 `.spkb` 走 `POST /import` 恢复。页面无需认证即可加载，API 调用仍走同一鉴权。

### 3.6 LAN 鉴权与会话

- 默认只绑 `127.0.0.1:8787`；要开 LAN 改 `bind = 0.0.0.0:8787` 并重启。非 loopback 没有 `auth_passwd` 时 daemon 降级 loopback 并报错。
- 密码通过 `svc lan passwd <plaintext>` 设置，明文不进配置文件；daemon 用 PBKDF2-HMAC-SHA256（随机盐，`pbkdf2-sha256$<iterations>$<salt>$<hash>`）写入 `svcd.conf` 的 `auth_passwd`，并立即清空已有会话。密码（**等价 root / Administrator**）只用于登录与 Bearer；无用户体系、无注册。
- `svc lan status` 显示 requested/effective bind、`authRequired` 和 `passwordSet`；`svc lan enable/disable` 已删除。
- 登录：`POST /login` 带 `{password}` → 设两个 cookie：`svcd_session`（Path=`/`）与 `svcd_api`（Path=`/api/v1/`），均 HttpOnly、SameSite=Strict、Max-Age=86400、无 Secure（无 TLS）；滑动续期 24h；`POST /logout` 清两个并撤销服务端会话。同一密码也可直接作为 `Authorization: Bearer` 访问 `/mcp` 与 `/api/v1/*`。
- LAN 开启时 `/plugin-ui/*` 与 `/rpc` 都要求会话；插件 UI 另外要求 per-plugin token（路径 `/plugin-ui/{id}/{token}/`，见 §3.5）；`/rpc` 保留 Origin 校验：只接受同源 Origin/Host、拒绝 CORS，cookie 调用必须带同源 `Origin`，但不要求 `X-Svcd-Csrf`（SameSite=Strict + 同源 Origin 承担 CSRF/DNS rebinding）；`/api/v1` 的 cookie 写仍要求 `X-Svcd-Csrf`。
- 登录失败固定 1s 延迟，连续 10 次锁 5 分钟（内存）。
- **厂商后台免登录**（ADR-0032）：`POST /vendor-login` 是认证入口，跑在准入之前。它只在 `Origin` 与 `Host` 同主机、端口为空 / 80 / 443（设备自己的 web 根）且带 `X-Vendor-OK: 1` 时，签发一对与 `POST /login` 同形的 `svcd_session` / `svcd_api` cookie；`OPTIONS` 预检回 CORS（`Allow-Origin` echo 调用方、`Allow-Credentials`、`Allow-Headers: X-Vendor-OK`）。服务端不代查厂商 SID —— 厂商会话绑客户端 IP，只有浏览器里的页面能判断，注入到厂商后台的菜单脚本负责先问厂商 `loginStatus` 再发这条请求。
- **v1 不支持 TLS**：核心不终止、不生成证书；要 HTTPS 就在前面放反代。明文密码过 LAN 的风险写进文档；默认关闭 LAN 是安全默认。
- 单用户 / 单角色；多用户、角色不做；HTTP 管理 API 见 §3.5。
- **实现状态（2026-11-15）**：`bind` 决定监听与认证（loopback 免密，非 loopback 必须有 `auth_passwd`）；`svc lan passwd` 写 `auth_passwd` 哈希并热更新，`svc lan status` 只读；`POST /login` 下发 `svcd_session`（Path=`/`）/`svcd_api` 两个 cookie；`/api/v1` 收 Bearer 密码或 cookie，cookie 写要求同源 `Origin` + `X-Svcd-Csrf: 1`；`POST /logout` 撤销服务端会话；登录失败 1s、10 次锁 5 分钟。`/mcp` 的 Bearer 面由 #40 实现（见 §3.4）；cookie 不参与 MCP 认证。

## 4. OSAL 与平台边界

**OSAL = 5 个边界 + 0 个 trait。**

| 边界 | 形状 | 平台差异 | 形态 |
| --- | --- | --- | --- |
| `proc` | `spawn / request_stop(polite / force) / wait_exit / liveness` | fork+subreaper vs CreateProcess+Job | cfg 模块 |
| `wait` | `wait(&[Handle], timeout) -> Ready` | poll(2) vs IOCP/线程 | driver |
| `channel` | `ControlChannel { bind / serve / connect / remove_socket / set_private }` | UDS vs 命名管道 + DACL | `channel/platform/{unix,windows}.rs`，编译期 trait（无运行时分发；#143） |
| `install` | `plan(os) -> InstallPlan { files, commands }` | 有，但是数据 | 纯数据 |
| `webview` | webview/webview 薄包装 | 无 | 桌面独占 |

- 零运行时 trait：同一构建只有一份实现、差异是一次性 syscall 映射，trait 没有运行时收益；#140 / #143 的编译期 seam（`ConnectionDriver` / `ControlChannel`）是单一实现的静态接口，不做 dyn 分发，可测性仍靠「纯决策核」（`Action` 枚举 + 派发），不靠 fake。
- Unix 与嵌入式共用一条同步循环（单线程，`wait` 唯一 OS 触点，timer 线性扫描，不引 mio/tokio）；Windows 桌面走独立 driver。
- Windows 两条事实外溢：`WaitForMultipleObjects` 64 句柄上限（每插件 3 个 ≈ 20 插件封顶）；管道没有「可读」可等待态，数据就绪靠 overlapped I/O 或线程。
- 事件只认 `ShutdownRequested / ChildExited / Inbound`；Unix 用 self-pipe（不用 signalfd），Windows 用控制处理器线程 → event。
- 优雅关闭策略平台无关：依赖逆序 → polite → 等 T → force；Windows 的 polite 降级为「控制帧 + 等 T + TerminateProcess」，写进规格，不被抽象抹平。
- `status` 只作 manifest 可选能力、不进 OSAL；`[auto]` 由 svcd 推断，不由服务自报。

## 5. CLI、status、config、logs

### 5.1 入口

- `svcd`：守护进程；`svcd run --foreground`、`svcd install-service / uninstall-service`、`svcd --install [--conf <file>] / --uninstall`。
- `svc`：服务 CLI，接受任意服务 id（core 或 plugin）：`list / status / start / stop / enable / disable / logs / lan / version`。
- `plugin`：插件 CLI，**只接受 `kind=plugin` 的 id**；动词与 `svc` 同源，另加 `install / uninstall / editconfig / export / import / where`。传 core 服务 id 报错「不是插件」。`plugin where <id>` 读位置索引回答「在哪个根、那卷在不在」（不依赖守护进程）；`plugin uninstall --force` 在根不可用时只删记录、不碰卷。
- `mcp`：stdio MCP 桥，每个客户端一个进程。
- `svc ui`：桌面 GUI 入口，客户端；关窗不退 svcd。

### 5.2 status

- `summary`：id、kind、state、enabled、pid、since、version；插件额外带 `displayName`（清单里的显示名）与 `installDir`（安装目录 `<根>/<id>`，即当前版本目录的父目录），core 服务不带这两个键；设备锁定的包（manifest 的 `compat.devices` 非空，如 `sr6110` 专属变体）再带 `devices`（该包接受的设备 id 列表），能跑在任何设备上的包不出现这个键——变体包与同平台的通用包共用同一个 `id` 和版本号，客户端只有靠它才能认出装的是哪一个（#350）。HTTP `GET /api/v1/services` 的投影额外带 `hasConfig`（活动配置是否存在）、`hasUI`（manifest 是否声明可打开的 web/gui UI）与 `installRootIndex`（安装目录所在根在 `plugins_dir` 里的序号，从 1 开始；嵌套根取最具体的那个），`svc list` 的 payload 不变。
- `details`：dependencies[]、last_exit、next_at、ready_since、failures、config、probe（仅 `svc status` 探测 core 的 status 动作时出现，内容为该动作的 stdout/摘要）。
- `svc list` 列：`ID  KIND  STATE  AUTO  PID  VERSION`；AUTO 显示 `[auto]` 或 `-`。
- `svc list` 只投影受管服务（core + plugin），不含内置 `svc` 自身（svcd 由系统服务管理器拉起，不归自己管）；`svc status svc` 仍可读。
- `svc status <id>`：逐字段块；`--json` 原样输出 `{summary, details}`。
- `[auto]` 只映射 `enabled`（开机启动）；崩溃重启策略放 details 的 `restart`（如 `on-failure`），不用 `[auto]`。
- 退出码：0 成功；1 服务不存在；2 守护进程不可达；64 参数错误。
- MCP 的 `svc.list` / `svc.describe` 用同一 payload，不另造字段。

### 5.3 config

- **只适用于 plugin**：core 的配置由脚本/bin 自己管，svcd 不提供 `config` / `editconfig` / `export`。
- 模板：`config` 字段，单文件、包内相对路径、默认 `config/config`、必须在 `files[]`；禁止绝对路径 / `..` / 符号链接 / 目录；显式路径缺失 = 安装失败；缺省且默认文件不存在 = 无配置。
- 活动副本：`<data>/plugins/<id>/config`，root:svcd 0640（无组 0600）；安装时缺则复制模板、存在则不覆盖；记录模板 sha256 到 `config.template.sha256`。
- 启动不覆盖、不自动重建；缺失就按无配置启动，不注入 `SVCD_CONFIG`；有配置时 svcd 注入 `SVCD_CONFIG` 给插件。
- 升级保留活动副本；模板 hash 变化写 `config.new`（0640）并警告；`--reset` 用模板覆盖（先备份 last-good）。
- `editconfig <id>`：要 root；flock `<id>/config.lock`；临时文件 → `$EDITOR`（fallback `vi`）→ 可选 `configCheck`（10s，候选路径，`SVCD_CONFIG`）→ fsync + 原子 rename，旧文件存 `config.last-good`；支持 `--print` / `--file` / `--reset`；不自动重启，打印「restart to apply」。
- 导出/导入：
  - `export --config-only`：原样活动副本，默认 `<id>-<version>.config`，0600。
  - `export`：`.spkb` 备份容器（ustar）：`backup.json`（首条目）`{schema:1,id,version,exportedAt,configSha256}` + `plugin.spk`（从已安装版本重建的签名包）+ `config`；容器不签名，导入验内层签名（`allow_insecure_install` 开启时允许内层未签名/未验签）。同一个开关也允许导出：未签名安装的版本导出为不带 `signature.json` 的 `.spk` / 内层 `plugin.spk`；开关关闭时仍拒绝导出。
  - `import <file>`：tar 首条目 `backup.json` → `.spkb`；`manifest.json` → `.spk`；其它 → 裸配置，必须 `--config <id>`。id 不符拒绝；版本不同警告；`--no-config` 跳过；`--force` 只跳过同版本检查，不跳签名。
- 配置上限 1 MiB；插件私有格式（JSON/TOML/YAML/ini 都行），svcd 不解析、不做 schema、不做升级字段合并；无 `configCheck` 时只保留 last-good。
- 权限：写（install / editconfig / import / uninstall）要 root；只读（status / logs / `editconfig --print` / `export`）组可用；配置明文，不加密（见 §13-C5）。
- **实现状态（2026-11-12，#34 / #39）**：安装复制模板 / `config.template.sha256` / 模板变化写 `config.new`；`editconfig`（`--print/--file/--reset`、`$EDITOR`、`configCheck` 10s、last-good、flock）；`export`（config-only / `.spkb`）与 `import`（`.spkb` / `.spk` / 裸配置自动识别）已实现。HTTP config/export/import 端点已由 #39 实现（`GET/PUT /config/{id}`、`POST /config/{id}/reset`、`GET /export/{id}` 与 `/export/{id}/config`、`POST /import`）；hooks 由 hook runner 执行（post-install 失败回滚，pre-remove 失败不阻塞卸载）。

### 5.4 logs

- 落点：`<logs>/<id>.log`；svcd 自身 `svcd.log`；hooks `<id>-hooks.log`。
- 行形状：`<RFC3339 UTC 毫秒> <LEVEL 5> <source> <message>`；source 为 `svcd` / `plugin:<id>` / `hook:<id>`，消息内 CR/LF 落成字面 `\\n`（ADR-0022）。
- 级别：`error` / `warn` / `info` / `debug` / `trace`，默认 `info`；低于阈值的行在写侧丢弃、不落盘。`log_level` 配置，非法值启动即错；`log = off` 关掉全部日志（文件与 stderr）。
- 轮转：64 KiB × 3（`.log`、`.log.1`、`.log.2`），丢最旧，不压缩，不引 logrotate；`log_max_bytes` 仍可覆盖。
- 权限：0640 root:svcd；`svcd` 组只读；插件 stdout/stderr 由 svcd 捕获后写，插件不能改。
- `svc logs <id> [--follow|-f] [--lines N] [--level L]`；`svc logs --all` 按时间戳合并日志文件；组可读；不做远程 syslog、不做 JSON lines。老格式行原样透出、不参与级别过滤。
- 嵌入式同一策略，`/var` 落 overlay，64 KiB × 3 控制 flash 写放大；掉电丢当前缓冲可接受。
- 不做日志汇聚、远程推送、journald / Event Log 双写。

## 6. 安全与权限

- **所有服务进程（core 脚本/bin、plugin）与 hooks 以 svcd 身份运行**：Linux / macOS = root，Windows = LocalSystem。不降权、不做每服务身份、不做沙箱、不设 `privilege` 字段。
- 服务之间没有身份隔离：可互读数据、互发信号；这是接受的取舍。core 定义无签名，信任来自 root 所有的 `services.d/*.json` 与本机管理员；plugin 信任来自 Ed25519 签名 + `trust.d`。
- 本地授权两档：
  - root / Administrator：install、upgrade、uninstall、`editconfig`、`import`、`svc lan passwd`。
  - `svcd` 组：start / stop / status / logs、`editconfig --print`、`export`（只读）。
- 两档由控制通道的两个端点实施（§3.1）：`svcd` 端点只接受组档方法，`svcd-admin` 端点只接受 root / Administrator。端点 ACL 即鉴权，请求不携带凭据。
- Windows 上 `svcd install-service` 建/校验 `svcd` 本地组，系统模式 daemon 启动时组缺失即失败；`uninstall-service` 不删组（误删成员比留一个成员更危险）。
- LAN 是另一套：管理员密码（`auth_passwd` 哈希）；UI 用 `svcd_session`，`/api/v1` 用 `svcd_api` 或 Bearer 密码，`/mcp` 只收 Bearer 密码；不映射 OS 用户。**HTTP 管理 API 覆盖全部 `svc` 命令，密码等价于 root / Administrator**；默认 LAN 关闭、无 TLS。
- 配置明文 0640 root:svcd；不接 Keychain / DPAPI / Secret Service / TPM、不加密；导出/备份含明文 secret，文档警告；插件自管密钥。
- **明确接受：任一服务/插件有 bug 或被攻破 = 整机 root / System**；补偿只在来源侧（core 的 root 文件权限、plugin 的签名/审核/`trust.d`）。

## 7. 构建、体积与 CI

- 语言：Rust 单语言；release profile：`opt-level=z` + fat LTO + `codegen-units=1` + `panic=abort` + strip（#16 骨架）。
- 目标：
  - 嵌入式：`x86_64` / `aarch64` / `armv7` + musl；
  - Windows：`x86_64-pc-windows-msvc`；
  - macOS：`aarch64-apple-darwin`、`x86_64-apple-darwin`（分包）；
  - Linux 桌面：`x86_64-unknown-linux-gnu`（webkit2gtk 只能动态链，**musl 不可行**）。
- 工具链：ubuntu-24.04 上用 cargo-zigbuild（pin `=0.24.1`）+ Zig（`0.17.1`）出三架构 musl；Windows/macOS 原生 runner；唯一高风险环是 armv7+musl 的 zig 链接。
- 计量：multicall 单二进制，strip 后、未压缩、十进制字节；硬链接算一次；编译进二进制的资源算；安装的 unit/config 与插件不算。
- 门禁：嵌入式 **≤ 2,000,000 B**、桌面 **≤ 30,000,000 B** 超限 fail；基线 +2% warning；`tools/measure_size.py` + `.github/workflows/size.yml` + `ci/size-baseline.json`。
- 实测基线（2026-11-12 本机）：musl x86_64 `1,119,152 B`；Windows x86_64 `910,848 B`；macOS / Linux gnu 待 CI 首跑。
- **30MB 只约束核心二进制**；安装包不计门禁，只记录体积（见 §13-C2）。

## 8. 桌面交付

| 平台 | 安装包 | 最低系统 | 架构 | WebView | 签名 |
| --- | --- | --- | --- | --- | --- |
| Windows | MSI（WiX + Burn） | Windows 11 build 22000+（测试 25H2 / 26H1 / 26H2） | x86_64（ARM64 后补） | Evergreen WebView2；缺失跑 bootstrapper（≈2MB） | Authenticode（硬件/云 HSM） |
| macOS | flat pkg（可套 dmg） | macOS 15 | arm64、x86_64 分包 | 系统 WKWebView | Developer ID + 公证 + staple |
| Linux | deb + rpm | Ubuntu 22.04（universe）/ Debian 12 / Fedora 43+ / Arch | x86_64 先出（aarch64 后补） | libwebkit2gtk-4.1-0（声明依赖） | 仓库 GPG |

- 系统级守护进程 + 专用组 `svcd`：安装器建组并把安装用户加入（Linux postinst 用 SUDO_USER、macOS pkg 取 console user、Windows MSI custom action 用 LogonUser；取不到则提示手动加组）。
- GUI 入口 `svc ui`：客户端；安装器建快捷方式 / `.desktop` / `.app` 包装；关窗不退 daemon。
- macOS：只有 pkg 能以 root 跑脚本装 LaunchDaemon；不用 SMAppService（绑 .app、CLI/无人值守更绕）；Info.plist `NSAllowsLocalNetworking`；spawn 子进程不需要额外 entitlement；插件是独立进程，不 dlopen 第三方 dylib；Apple Developer $99/年。
- Windows：MSI 每机器安装（ALLUSERS=1 + ProgramFiles64Folder），WiX `ServiceInstall`/`ServiceControl` 注册 SCM；Burn bundle 带 WebView2 bootstrapper；不选 MSIX（包身份/按用户模型不贴每机器守护进程）、不选 NSIS（无 MSI 修复/卸载策略）；Win10 已 EOL，不写进支持矩阵；SmartScreen 是信誉制。
- Linux：webkit2gtk-4.1 基线；放弃 RHEL 9（只有 4.0）；只出 deb + rpm，不出 AppImage / Flatpak / Snap；桌面 Linux 必须 gnu target；自建 apt 仓库必须签 Release/InRelease，rpm 仓库 `gpgcheck=1`。
- 发布前置：Apple Developer 账号、Windows 代码签名证书 + 硬件/云 HSM、CI 的 macOS 签名/公证与 Windows 签名 job、Linux 打包与仓库签名。

## 9. 嵌入式目标

- 版本：**当前 stable + 上一 stable** 两条 OpenWrt 线；发布时在 `targets.toml` pin 具体版本，EOL 即移出；不承诺更老版本。
- 架构：x86_64、aarch64、armv7（与 CI musl 三架构一致）；mips 不承诺。
- libc/内核：musl；kernel ≥ 5.15；procd 为默认，BusyBox init 保留 best-effort（五份安装计划）。
- 存储：overlayfs；`/var` 落 overlay（持久），不用 tmpfs 存日志/配置；安装后至少留 8 MiB 给插件 + 日志 + 配置。
- 包管理：按目标版本实际用 opkg 或 apk；svcd 自身打 ipk / apk 包。
- 功能面：UI 与 `mcp` 保留；TLS 不链；不编 WebView。
- CI：musl x86_64/aarch64/armv7 + 2,000,000 B 门禁；`targets.toml` 驱动。
- 非目标：mips、非 Linux 嵌入式、OpenWrt 之外的 BusyBox 系统（best-effort，不承诺）。

## 10. 自升级

- **v1 不做 svcd 自升级**；升级交给平台包管理器（apt/dnf、MSI、pkg、opkg/apk）。
- 不内置更新器、不联网检查版本、不自动替换二进制；`svc version` 只报版本。
- 签名沿用包签名（Authenticode / 公证 / deb 签名 / 仓库 GPG），不做第二套 Ed25519 自更新。
- 服务重启由包管理器脚本负责；插件进程不随 svcd 重启，按 §2.4 下次 start 生效。
- 回滚 = 包管理器装回旧版本；不维护 A/B 分区。
- 兼容：svcd 升级后按 `protocol` / `platform` 校验旧插件；不兼容的拒绝 start 并提示升级插件。

## 11. 兼容与迁移

- `platform` 精确匹配；Rosetta 与 Windows ARM64 的 x64 模拟留作后续放宽，不默认开。
- `protocol: 1` 精确匹配；主版本内加法兼容；未知方法 `-32601`，未知字段忽略。
- `config` / `configCheck` 为加法字段；旧 svcd 忽略它们（版本偏斜，文档写明）。
- `arch` 不是合法字段：v1 的 `schema: 1` 已重写为只含 `platform`；不提供 arch 兼容层。
- manifest 格式已重编号（#260）：只有 `schema: 1`，`lifecycle` / `interpreter` / `compat` / `icons` / `capabilities[].outputSchema` 全在 1 内；旧线任何版本的包都缺必填 `id`，会在验包阶段被拒绝并提示重打包。不提供读入兼容或 id 迁移。
- `id` 变了就是新插件：按 `id` 落目录，旧目录不迁移；改 `id` 前先 `plugin uninstall`。显示名允许重复。
- 插件升级：默认 `replace` 采用 §2.4 的「空间检查 → 备份/还原或删旧」语义；显式 `--strategy keep-old` 走原子并存；活动配置在版本目录外，先删旧版本不会丢。

## 12. 票与笔记索引

| 票 | 标题 | 结论一句话 |
| --- | --- | --- |
| [#2](https://github.com/webxxxxx/plugin-manager/issues/2) | 插件 IPC 与反向代理栈 | NDJSON 一套分帧跨 stdio/UDS/命名管道；Windows 稳定版无 UDS；musl 空壳 364.7KiB、axum 748.8KiB |
| [#3](https://github.com/webxxxxx/plugin-manager/issues/3) | 进程监督内核与四平台服务管理器对接 | s6 不可复用；自研抄 run/finish 与二分支退避，以 BusyBox 为基线 |
| [#4](https://github.com/webxxxxx/plugin-manager/issues/4) | 插件 IPC 协议形态 | JSON-RPC 2.0 NDJSON；1MiB/64/30s；stdio 默认；无握手 |
| [#5](https://github.com/webxxxxx/plugin-manager/issues/5) | MCP 协议现状与内嵌聚合 Server | 规范 2026-07-28；聚合归聚合方；自研最小 stdio 层 |
| [#6](https://github.com/webxxxxx/plugin-manager/issues/6) | MCP 聚合层对外形态 | 语义在 svcd、协议在桥；dual-era；ttl/list_changed；isError |
| [#7](https://github.com/webxxxxx/plugin-manager/issues/7) | Rust 体积账本 | 单二进制成立但很窄；TLS 必须外置；Go 出局 |
| [#8](https://github.com/webxxxxx/plugin-manager/issues/8) | 桌面 WebView 方案 | webview/webview；系统引擎；嵌入式不编译即零残留 |
| [#9](https://github.com/webxxxxx/plugin-manager/issues/9) | 服务模型与 IPC 端点 | PM 退役；core=脚本/bin，plugin=.spk；svc list 与 tools/list 同源 |
| [#10](https://github.com/webxxxxx/plugin-manager/issues/10) | CI 交叉编译与体积测量 | cargo-zigbuild 0.24.1 + Zig 0.17.1；strip 后字节；2% 告警 |
| [#11](https://github.com/webxxxxx/plugin-manager/issues/11) | 体积预算分配 | 嵌入式放宽到 2MB；单二进制 + 三硬链接；桌面仅 webview feature |
| [#12](https://github.com/webxxxxx/plugin-manager/issues/12) | WireGuard .spk 样张 | 格式成立；12,800 B；协议跑通；平台标识缺口转 #18 |
| [#13](https://github.com/webxxxxx/plugin-manager/issues/13) | .spk 包格式与安装布局 | 严格 tar；Ed25519 签 manifest；keep-old/replace；hooks 60s |
| [#14](https://github.com/webxxxxx/plugin-manager/issues/14) | 监督器实现 | 6 态纯核；退避 1/2/4/8/16/30s；probe-on-spawn；五份安装计划 |
| [#15](https://github.com/webxxxxx/plugin-manager/issues/15) | OSAL 抽象清单 | 5 个边界 + 0 trait；Unix/嵌入式同步循环，Windows 独立 driver |
| [#16](https://github.com/webxxxxx/plugin-manager/issues/16) | CI 尺寸探针 | measure_size.py + size.yml；2MB/30MB fail；实测 504,544 / 327,168 B |
| [#17](https://github.com/webxxxxx/plugin-manager/issues/17) | 桌面构建裁剪与交付 | 系统级 daemon；pkg/MSI/deb+rpm；30MB 只量二进制；三平台签名 |
| [#18](https://github.com/webxxxxx/plugin-manager/issues/18) | manifest 平台标识 | arch → platform 三元组；安装+加载双检；arch 只派生 |
| [#19](https://github.com/webxxxxx/plugin-manager/issues/19) | 插件身份与权限边界 | 全部 root/System；不降权、不设 privilege；装/升/卸要 root |
| [#20](https://github.com/webxxxxx/plugin-manager/issues/20) | config 字段与配置流 | config/config 模板；活动副本 0640；editconfig 要 root；.spkb |
| [#21](https://github.com/webxxxxx/plugin-manager/issues/21) | 依赖版本约束 | v1 不做版本约束；id 列表；start 按拓扑拉起，缺则拒绝 |
| [#22](https://github.com/webxxxxx/plugin-manager/issues/22) | svc status 输出 | summary+details；[auto]=enabled；退出码 0/1/2/64 |
| [#23](https://github.com/webxxxxx/plugin-manager/issues/23) | 日志落点与轮转 | per-service 文件；1MiB×3；不接平台日志；组只读 |
| [#24](https://github.com/webxxxxx/plugin-manager/issues/24) | LAN 鉴权与会话 | 默认 loopback；管理员 token；UI/API cookie；MCP Bearer；无 TLS；单用户 |
| [#25](https://github.com/webxxxxx/plugin-manager/issues/25) | svcd 自升级 | v1 不做；交给平台包管理器；不维护 A/B |
| [#26](https://github.com/webxxxxx/plugin-manager/issues/26) | 嵌入式目标平台 | OpenWrt stable 两条线；三架构 musl；kernel≥5.15；overlay |
| [#27](https://github.com/webxxxxx/plugin-manager/issues/27) | 配置敏感值保护 | 明文 0640；不做 keyring/加密；插件自管密钥 |

研究笔记：

- [插件 IPC 与反向代理栈](docs/research/2026-11-12-plugin-ipc-and-reverse-proxy.md)
- [MCP SDK 与聚合](docs/research/2026-11-12-mcp-sdk-and-aggregation.md)
- [进程监督与四平台服务管理器](docs/research/2026-11-12-process-supervision-and-service-managers.md)
- [嵌入式 1MB 运行时可行性](docs/research/2026-11-12-embedded-1mb-runtime-feasibility.md)
- [CI 交叉编译与体积测量](docs/research/2026-11-12-ci-cross-compile-and-size-measurement.md)
- [桌面 WebView 方案](docs/research/2026-11-12-desktop-webview-options.md)
- [桌面发行基线](docs/research/2026-11-12-desktop-packaging-and-platform-baselines.md)
- [插件降权与隔离机制](docs/research/2026-11-12-plugin-privilege-mechanisms.md)
- 其他：[GLOSSARY.md](GLOSSARY.md)、[ADR 0001](docs/adr/0001-single-multicall-supervisor.md)

## 13. 冲突、缺口与待决

> 这些不是新的产品取舍，是 26 张票合到一起后暴露的**不一致或没覆盖到的地方**。按优先级处理。

| # | 类型 | 问题 | 证据 | 建议 |
| --- | --- | --- | --- | --- |
| C1 | **冲突（已定）** | manifest `schema: 1` 仍是 #13 写的、含 `arch`；#18 把 `arch` 换成 `platform`，属破坏性字段替换，但没有 bump schema 版本。旧解析器读新包会因缺 `arch` 失败，新解析器读旧包会因缺 `platform` 失败 | #13 Answer / 评论、#18 Answer | **已定（2026-11-12）**：删除 `arch`，只留 `platform`；`schema: 1` 重写，不提供 arch 兼容层（尚无外部稳定包；若已发过再 bump 2）。 |
| C2 | **缺口（推迟）** | CI 只构建 3 个 musl + Windows，nightly 只加 `aarch64-apple-darwin`；#17 要求 Linux 桌面 `x86_64-unknown-linux-gnu`、macOS `x86_64-apple-darwin`，且没有 MSI / pkg / deb / rpm 的打包 job。桌面二进制与安装包都没被门禁覆盖 | #16 `ci/README.md`、#17 Answer | **推迟（2026-11-12）**：本轮不处理；做打包流水线时再加 gnu x86_64 / macOS x86_64 与各平台打包 job；安装包体积先记录、不门禁。 |
| C3 | **待决（已定）** | MCP 原先只有本地 stdio 桥；#11 留了「要的话单开票」，之后没有票；#24 的 LAN 鉴权只覆盖 UI / `/rpc`，没有 `POST /mcp` | #11 Answer、#24 Answer、#6 | **已定（2026-11-12）**：HTTP 增加 `POST /mcp`，modern-only Streamable HTTP、JSON、无 session/SSE、Bearer；legacy 走 stdio 桥（§3.4）。 |
| C4 | **缺口（已定）** | HTTP 面原先只有插件 UI 的 `/rpc`；没有 HTTP 管理 API（list/start/stop）。嵌入式浏览器用户无法管理插件，只能 SSH/CLI | #4 UI 通道、#24 Answer | **已定（2026-11-12）**：HTTP 增加 `/api/v1` 管理 API，覆盖全部 `svc` 命令（services / install / import / export / config / logs / lan / version）；Bearer 或 cookie，写操作 CSRF；token 等价 root，默认 LAN 关闭、无 TLS（§3.5 / §6）。 |
| C5 | **边界（已定）** | TLS 不在核心；#24 的 token 明文过 LAN；#17 的 macOS ATS 只是本地回环例外。这是已接受的取舍，不是 bug | #11、#24、#17 | **已定（2026-11-12）**：v1 不支持 TLS，不生成证书；HTTPS 靠前置反代；LAN 只用于可信网络；token 等价 root，默认 LAN 关闭。不要在核心里偷偷加 TLS。 |
| C6 | **已澄清** | 组权限：#17 说组管生命周期；#19 说组只做 start/stop/status/logs；#20 又给了只读 `export` / `editconfig --print` | #17、#19、#20 | 以 §6 的授权矩阵为准：组只读 + 生命周期；写操作 root；#19 已补评论 |
| C7 | **平台例外** | #4 方法集有 `svcd.shutdown`；#14 说 BusyBox 上不提供 `svcd shutdown` | #4、#14 | 规格保留方法，但注明 BusyBox 不实现；插件必须能接受「不发 shutdown 直接 SIGTERM」 |
| C8 | **实现约束（已定 2026-11-17）** | Windows 命名管道默认 DACL 给 Everyone 读；#17/#19 要求组授权，不能吃默认值 | #15、#2 笔记 | 控制通道分两个端点（`svcd` / `svcd-admin`），各设一条显式 protected DACL：`svcd` = SYSTEM/Administrators 完全 + `svcd` 组读写，`svcd-admin` = 仅 SYSTEM/Administrators；不依赖默认值。配套删除 `svcd.token`，鉴权即端点 ACL（§3.1 / §6）。 |
| C9 | **边界（已澄清）** | 30MB 只量核心二进制，安装包不计；#11/#16 的预算表容易让人以为含安装包 | #11、#16、#17 | 文档统一写「核心二进制」，安装包体积单列记录 |
| C10 | **文档债** | #13 的正文/Answer 仍写 `arch`；#18 只在评论里回填；样张已同步但 schema 文档未重生 | #13、#18、#12 | 以本规格 §2.2 为准，重新生成 manifest schema 文档与样张说明 |
| C11 | **缺口（中）** | `svc ui` 只定了「入口」，没定界面内容；桌面商城客户端在 Out of scope，核心 UI 显示什么没写 | #17、地图 Out of scope | 明确 v1 `svc ui` 只做「本地服务列表 + 打开插件 UI」；商城另立 |
| C12 | **待定（低）** | OpenWrt 的「stable 两条线」还没 pin 具体版本；`targets.toml` 未落地 | #26 | 发布前 pin 版本并进 CI；EOL 即移出 |
| C13 | **基线缺口** | 尺寸基线只有 musl x86_64 与 Windows x86_64；macOS arm64 / x86_64、Linux gnu 没有首跑数据 | #16 | CI 首跑后补 `ci/size-baseline.json` |
| C14 | **语义澄清** | #15 说 `[auto]` 由 svcd 推断；#22 说 `[auto]` = enabled。两者可兼容，但文档要写死 | #15、#22 | 以 §5.2 为准：`[auto]` = svcd 推断的开机自启，不是插件自报，也不是重启策略 |
| C15 | **命名冲突（已解）** | 票面默认「包根目录 `config` 文件」，但 #13 的包里 `config/` 是目录，不能同名共存 | #13、#20 | 默认改为 `config/config`；显式字段可指向任意 `files[]` 内普通文件 |
| C16 | **已澄清（服务模型根）** | 原服务模型只写 kind core/plugin，没定义 core 怎么来；core 容易被误当成 .spk 的一种 | #9、#13、#14 | **已定（2026-11-12）**：core = `services.d/*.json` 或 `services.d/<id>.sh|bat|cmd|ps1` 的脚本/可执行文件；plugin = 标准 `.spk`；两者共用监督/依赖/日志，但能力面只归 plugin；install/config/export 只对 plugin（§1.6 / §1.7）。**2026-11-13 补充**：schema 2 的 plugin 也可声明 `lifecycle`，复用 core 的 start/stop/status 执行路径；来源与信任边界不变（§1.8）。 |

### 仍未处理

- **C2（推迟）**：桌面 Linux gnu、macOS x86_64、各平台安装包仍不在门禁里；30MB 目前只对 Windows 有意义。维护者 2026-11-12 决定往后放，实施打包流水线时再补。
- **C11（未定）**：`svc ui` 的界面内容；C12 / C13 是低优先的版本 pin 与基线补齐。
- C1 / C3 / C4 / C5 已在 2026-11-12 定；C6 / C7 / C8 / C9 / C10 / C14 / C15 已澄清或属实现约束。

## 14. 不在本规格

- 商城（Web/桌面客户端、审核、签名分发、按架构过滤）——见地图 Out of scope。
- ipk2spk、AI 建插件流水线——见地图 Out of scope。
- TLS 终止、多用户/角色、配置加密、svcd 自升级——v1 不做；要做单开票。

## 15. 修订记录
- 2026-11-28（#342）：能力可以挂一次性动作。manifest `capabilities[].hook = {path, args?, env?, timeout?}`：宿主每次 `tools/call` 起一次该动作，参数写 stdin，stdout 原文是这次调用的文本结果（与通道那侧 handler 的 stdout 同形，不解析 JSON）；stderr 进 `<id>-hooks.log`。结果超过契约内联上限就是这次调用失败，不截断成更短的答案。一个能力只有一种投递方式——`ready=notify` 不许声明 `hook`，`ready=exec`/`port` 必须给每个能力声明 `hook`。这样「一个常驻进程 + 偶尔被调一次」的插件不必为了能力面再养一个通道 shell（ADR-0002 同日记了这条修订）。


- 2026-11-23（#267）：复审收口。`GET/PUT /api/v1/settings`：派生细项（`max_*`、队列、栈、`ws_inflight_*`）只读展示、PUT 拒绝；值必须是单行，写前先构造并解析整份文档、只允许点名的键变化（挡住「值里塞一行只读键」）。控制台失败直接落盘前校验。`svcd.conf` 编辑改成逐字节保留（CRLF、注释、缺尾换行）。`log clear` 的审计行写进被清空的插件日志（不再是 daemon 日志）。安装与 services.d 撞 id 由注册表给 `duplicate service id ... (display name ...)`，状态 409。`SVCD_*` 只在 bootstrap 读一次，面层报来源用快照（ADR-0019）。`svcd upgrade` 把预发布 tag（带 `-`）当不更新。协议 3：stream 0 队列满只丢帧不杀实例（调用侧按 pending 背压/超时收口），单条 raw 流超限关闭该流。`check_spk.mjs` 的 manifest 规则可注入自测（`--self-test`），协议 3 + capabilities 重新合法。
- 2026-11-23（#266）：协议 3 增加**控制流** `0xFFFF_FFFF`，payload 是一个 JSON 对象，目前只有 `{"op":"close","stream":N}`。`/raw` 订阅者断开时宿主给插件发 close；插件发 close 时宿主给该流的 WebSocket 发干净关闭（1000）并释放订阅与在途预算；插件在已关闭的流上再写数据记 `stream_not_subscribed` 并回一个 close（不再静默丢弃）。数据流 payload 语义不变（空 payload 仍是空数据帧），控制帧 ≤128 字节，非法控制帧记 `control_frame_invalid`，不占用 stream 表、不计入 raw 数据字节。
- 2026-11-23（#265）：协议 3 变成多路复用传输——`u32 len | u32 stream | payload` 的 **stream 0 就是 JSON-RPC 通道**（payload 是一条 JSON-RPC 消息，外层长度即帧界），stream ≥1 仍是 raw 二进制流。于是协议 3 插件可以 `ready: notify`、应答 `svcd.ping`、声明 capabilities，并同时提供 SSE、页面 WS 与 `/raw`；`ready` 与 capabilities 的既有关系（`ready=exec` 不能声明能力）不变。协议 1/2 不变。非协议 3 插件请求 `/raw` 现在返回 409 `raw_unavailable` 并指明需要协议 3；stream 0 超限/非 UTF-8/队列满记 JSON 通道失败（`json_frame_too_large` / `json_channel_full`），单条 raw 流超限会关闭该流（订阅者收到关闭事件，插件收到控制帧），实例继续运行。`ws_raw_enabled` 仍是最外层开关。

- 2026-11-23（#264）：`svcd version` 与 `svcd upgrade`（离线 `--file` / 在线经 plugin-store 的 `release_asset`，校验 `SHA256SUMS`，不验签）；控制台「检查更新」；Windows 自替换用「运行中的 exe 改名 `.old` → 落新文件 → 下次启动清理」，Unix 直接 rename。
- 2026-11-23（#263）：控制台设置面——`GET/PUT /api/v1/settings`（每项带来源与生效值、只写改动过的键、校验后写盘）、`svcd restart`（user mode 经 detached helper 换进程；系统服务模式拒绝并提示服务管理器）、保存后自动重启。

- 2026-11-12：C1 `platform` only、删除 `arch`，`schema: 1` 重写；C2 推迟；C3 增加 `POST /mcp`；C4 增加 `/api/v1` 管理 API（覆盖全部 `svc` 命令）；C5 明确 v1 不支持 TLS。
- 2026-11-12（补）：core/plugin 边界——core = `services.d/*.json` 的启动脚本/可执行文件，plugin = 标准 `.spk`；install/config/export 只对 plugin。
- 2026-11-12（补）：core 退出工具面——定义不再接受 `capabilities`；新增可选 `lifecycle.start/stop/status`，`svc start/stop/status` 调用钩子；`details` 增加 `probe`。
- 2026-11-12（补）：脚本服务 drop-in——`services.d/<id>.sh|bat|cmd|ps1`，`start/stop/status` 参数入口；同 id JSON 优先；`.ps1` 走 ExecutionPolicy Bypass。
- 2026-11-13（补）：脚本动作改为可选——脚本声明哪个动作，svcd 才调用哪个；缺 `start` 从头执行、缺 `stop` 由框架终止进程、`status` 只把 stdout 作为 `details.probe` 的细节补充。
- 2026-11-13（补）：服务树即进程组——Unix 独立进程组、Windows per-service job object；组内任一进程存活即 `Running`，stop 终止整棵树。
- 2026-11-13（补）：信任根可管理——`svc trust list|add|remove`、`GET/POST/DELETE /api/v1/trust`、Web 控制台上传 `.pub`；包内不携带信任根。
- 2026-11-22（补）：安装器内置 store 发布钥，`svcd --install` 写入 `trust.d/signing.pub`；商城不再自己分发公钥。
- 2026-11-13（补）：脚本插件——`schema: 2` 增加 `lifecycle` / `interpreter`；脚本 payload 必须声明解释器（版本可空），安装时解析并校验，运行时统一经解释器执行；core drop-in 脚本可原样加 manifest 转成 plugin；`plugin bundle --from-core` 自动生成 lifecycle；老 daemon 拒绝 schema 2。
- 2026-11-23（#260）：manifest 格式重编号为 `schema: 1`（唯一版本、不读旧线任何版本）。新增必填 `id`（身份）与 `name`（显示名，可中文）、`capabilities[].outputSchema`、顶层 `icons`；`lifecycle` / `interpreter` / `compat` 并入 1，取消 1/2/3 分级门禁；`interpreter` 由「脚本必需」改为「可选，声明了就必须有脚本」。`plugin bundle` 恒写 `schema: 1`，`--id` 写身份、`--name` 写显示名。
- 2026-11-17：控制通道删除 `svcd.token`，鉴权改为端点 ACL；本地两档由 `svcd` / `svcd-admin` 两个端点实施；C8 改为两端点各一条显式 protected DACL。
- 2026-11-26（#323）：生命周期语义翻转——`exec` 成为唯一的被监督进程，`lifecycle.start` 改为前置准备（必须退出，失败即启动失败），`lifecycle.stop` 改为进程消失后的收尾（`Stopped` 以它返回为准），终止权归宿主。core 脚本声明的 `start` 改为写进 `exec.args`，`plugin bundle --from-core` 同步。老包（`start` 是常驻进程）会以「start 动作超时」启动失败，错误日志指明把进程移到 `exec.args`。
- 2026-11-27（#335）：补上 spawn 之后、Running 之前的空窗口——`lifecycle.started` 在就绪后跑一次、失败即启动失败；`stop` 钩子新增 `SVCD_STOP_REASON` / `SVCD_LAST_EXIT`，让一个钩子按「为什么结束」分支。设计见 `docs/specs/2026-11-27-daemon-plugin-lifecycle-gaps.md`。
- 2026-11-27（#337）：第三种就绪语义 `exec.ready = "port"`——宿主在 `exec.probe_addr`（默认 `127.0.0.1`）上连 `exec.port`，能建连才算 Ready；spawn 前先试绑一次，端口被别人占着直接启动失败并点名端口。只对 TCP 有效，`ready=port` 不建协议通道（因此不能声明 capabilities），探测线程不阻塞监督循环。
- 2026-11-27（#332，ADR-0031）：服务树不再只是进程组——停止开始时按 `/proc` 父链快照 `setsid` 出去的子孙，polite / kill 两段都对「组 + 快照」一起发信号；卸载一个正在跑的 sshd 不再留下 dropbear 连接子进程。

## 16. 实施票（2026-11-12 拆出）

由本规格拆成 **16 张 tracer bullet**，全部 `ready-for-agent`，阻塞边用 GitHub 原生依赖表示，frontier 起点只有 [#28](https://github.com/webxxxxx/plugin-manager/issues/28)。

| 票 | 交付 | 阻塞于 | 骨架现状（尺寸夹具） |
| --- | --- | --- | --- |
| [#28](https://github.com/webxxxxx/plugin-manager/issues/28) | 注册表与 status | — | 已实现：services.d 扫描、status payload、CLI 接线 |
| [#29](https://github.com/webxxxxx/plugin-manager/issues/29) | 前台监督 core 服务 | #28 | 已实现：proc/wait、事件循环、定时器、UDS/命名管道管理通道；stderr 继承，日志归 #35 |
| [#30](https://github.com/webxxxxx/plugin-manager/issues/30) | 安装/运行 .spk（含 `plugin` 入口） | #28 #29 | 验签/`files[]`/解包/布局/插件扫描/`plugin` 入口已实现；hooks 已实现（60s、环境清空、post-install 按策略回滚）；`install -` / downgrade / prune 未做 |
| [#31](https://github.com/webxxxxx/plugin-manager/issues/31) | 插件 IPC 与工具面 | #30 | 已实现：NDJSON 分帧（1 MiB）、id 路由、64 在途、30s 取消、`registry.tools` / `registry.call`；崩溃摘能力并失败在途调用；128 排队未做 |
| [#32](https://github.com/webxxxxx/plugin-manager/issues/32) | `mcp` 桥 dual-era | #31 | 已实现：modern `server/discover`/`_meta`/缓存字段；legacy `initialize`/`ping`/`list_changed` 轮询；连不上 svcd exit 1 |
| [#33](https://github.com/webxxxxx/plugin-manager/issues/33) | Web UI 反代 | #30 #31 | 已实现：静态资源、base 注入、ETag/304、SPA fallback、`/rpc` 桥；Range/WS/SSE 不做 |
| [#34](https://github.com/webxxxxx/plugin-manager/issues/34) | 配置流 | #30 | 模板/活动副本/`config.new`/`editconfig`/`configCheck`/`.spkb` 导入导出已实现；HTTP 端点归 #39 |
| [#35](https://github.com/webxxxxx/plugin-manager/issues/35) | 日志与 `svc logs` | #29 | 无 |
| [#36](https://github.com/webxxxxx/plugin-manager/issues/36) | 安装为系统服务 | #29 | 无 |
| [#37](https://github.com/webxxxxx/plugin-manager/issues/37) | HTTP 管理 API | #29 #30 | 服务/插件/版本端点已实现；loopback、无 auth；#38/#39 待补 |
| [#38](https://github.com/webxxxxx/plugin-manager/issues/38) | LAN 鉴权与会话 | #37 | token/登录/cookie/CSRF/限速与 listener 重绑已实现；`/mcp` 归 #40 |
| [#39](https://github.com/webxxxxx/plugin-manager/issues/39) | HTTP 配置/导入导出/日志 | #34 #35 #37 | 无 |
| [#40](https://github.com/webxxxxx/plugin-manager/issues/40) | HTTP `/mcp` | #32 #38 | 已实现：modern-only `POST /mcp`、镜像头/Bearer/Origin/1 MiB 校验、与 stdio 共用 `serve_modern`；无 session/SSE |
| [#41](https://github.com/webxxxxx/plugin-manager/issues/41) | 桌面 `svc ui` | #33 #37 | 无 |
| [#42](https://github.com/webxxxxx/plugin-manager/issues/42) | 发行包与签名 | #41 | 无 |
| [#43](https://github.com/webxxxxx/plugin-manager/issues/43) | 嵌入式发行 | #29 #33 | 无 |

> 后续追加：[#57](https://github.com/webxxxxx/plugin-manager/issues/57) `svcd --install / --uninstall` 自安装/卸载（复制四入口 + 调用 install-service；默认保留数据，`--purge` 清理）。

**已打通的端到端路径**：`services.d/*.json` → `svcd run --foreground` → 真实进程（spawn / 就绪探针 / 退避 / stop）→ `svc list/status/start/stop` 经 UDS（Unix）/命名管道（Windows）；插件安装/IPC/工具面、MCP 桥（stdio 与 HTTP）与 HTTP 管理 API 已接通。其余行的系统服务安装、发行、桌面与嵌入式仍是骨架。
