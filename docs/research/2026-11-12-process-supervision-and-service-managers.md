# 自研监督器 vs 复用 s6，以及 svcd 对接四平台服务管理器的准确语义

- 日期：2026-11-12
- 问题：svcd 不做 PID 1，被 systemd / launchd / Windows SCM / BusyBox init / procd 保活。监督逻辑自己做，还是复用 s6？
- 方法：只采信一手来源（官方文档、官方 API 参考、上游源码）。标注「实测」的条目是在本仓库环境里跑出来的；没有实测的一律按「文档」或「推断」标出。
- 结论适用范围：嵌入式 musl 静态 ≤1MB，跨 Linux/macOS/Windows。

## 结论（先给答案）

1. **s6 不能当库链接，只能当外部二进制分发。** `libs6` 只有 4 个模块——accessrules、ftrigr、ftrigw、fdholder（[libs6 文档](https://skarnet.org/software/s6/libs6/)），没有任何 supervisor 接口。`s6-supervise` 是一个独立进程，靠服务目录里的文件 + `s6-svc` 发信号来控制。所以「复用 s6」的真实含义是：vendor 一组二进制 + 为每个服务生成 `run`/`finish` 脚本，而不是加一个 Cargo feature。

2. **s6 的「非 PID 1」模式正好就是 svcd 现在的位置，官方文档直接给出了和 svcd 同构的做法。** s6 明确说：只要 supervision tree「rooted in process 1」，即由外部 init 以 `respawn`/`Restart=always` 保活 `s6-svscan` 即可（[s6-svscan-not-1](https://skarnet.org/software/s6/s6-svscan-not-1.html)）。该页同时给出 systemd unit、inittab、Upstart、`/etc/ttys` 四种接法，并警告一个真实代价：**service 与 logger 之间的管道由 s6-svscan 维护，s6-svscan 一死管道就断、日志会丢**。

3. **四平台「重启策略 + 崩溃循环防护」是量级差，不是参数差。** systemd 是 `Restart=` 七值枚举 × 退出原因表 × `StartLimitBurst/IntervalSec`（默认 5 次 / 10 秒）；procd 是 `respawn <threshold> <timeout> <retry>`（默认 `3600 5 5`，`retry=0` 表示无限重试）；launchd 只有 `KeepAlive` 条件字典 + `ThrottleInterval`（默认 10 秒一次）；**BusyBox init 的 respawn 明确「不阻止进程失控地 respawn」**；Windows 的恢复动作在 SCM 侧、安装时用 `ChangeServiceConfig2` 配置，服务进程运行时改不了。想用一套自研退避覆盖全部，必须按平台取「最弱的那一层」当基线——也就是 BusyBox。

4. **非 PID 1 的子进程回收是分平台硬问题，没有可移植解。** Linux 用 `PR_SET_CHILD_SUBREAPER`（Linux 3.4+，systemd 自己就这么做）；macOS **没有**等价物（`prctl(2)` 的 STANDARDS 明确写 Linux），只能靠 process group + launchd 的 `AbandonProcessGroup` 语义；Windows 用 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`），根本不涉及 reopen/wait 回收。

5. **就绪通知可以零依赖实现，不需要链接 libsystemd。** systemd 在 `sd_notify(3)` 手册里直接给了 MIT-0 的独立 C 实现（自己 `connect()` 到 `$NOTIFY_SOCKET` 打一条 datagram），并声明该协议受 [PORTABILITY_AND_STABILITY](https://systemd.io/PORTABILITY_AND_STABILITY/) 稳定性承诺保护。对 ≤1MB 静态目标这是关键利好——静态 musl 下链 libsystemd 基本不可行。

6. **Windows 没有 notify 协议，「就绪」等于 `SERVICE_STATUS.current_state = RUNNING` 加上 `WaitHint`/`CheckPoint` 进度机制。** 且服务进程必须做「我到底是被 SCM 拉起的还是控制台里跑的」探测：`StartServiceCtrlDispatcher` 在控制台模式下返回 `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`，微软明确建议据此跳过服务专有代码——这正好是 `svcd run --foreground` 与 `svcd install-service` 双模式的官方标准做法。

7. **崩溃循环防护的「落点」每平台不同：能不能白拿，决定了自研要不要写退避。** systemd、procd、Windows（recovery actions）都在管理器侧，svcd 不写也有；s6 在 `finish` 脚本侧（`s6-permafailon <secs> <deathcount> <events>` 退 125 → supervisor 不再重启）；**BusyBox 完全没有**；launchd 只有 10 秒节流。自研监督器若不内置指数退避 + 失败上限，在 BusyBox 上必然变成失控循环。

8. **≤1MB 静态二进制：能塞下「s6 式监督逻辑」，塞不下「s6 生态」。** s6 式监督的本质是一个极小的异步状态机（一服务一进程：fork/exec → waitpid → 起 `finish` → 最小 1 秒间隔重启），`s6-supervise` 本身不 malloc、每实例内存极小。而 Alpine 上 `s6` 包 installed size 是 **1006.5 KiB**（60+ 个 applet + execline + skalibs 依赖），`s6-static` 的 143.6 KiB 只是**静态库**不是静态二进制。结论：逻辑可以自研进 1MB；整包 vendor 会把预算吃光。

## 证据

### s6：体积、许可证、非 PID 1 用法、可链接性

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| s6 许可证 ISC；当前版本 2.15.1.1；构建期需 skalibs ≥2.16.0.0，execline 为「强烈推荐」的构建期依赖（默认开启时也是部分二进制的运行期依赖） | 许可证对静态闭源/嵌入友好；依赖链是 skalibs（+ 默认 execline） | https://skarnet.org/software/s6/ | 2.15.1.1，2026-11-12 抓取 |
| `libs6` 模块只有 accessrules / ftrigr / ftrigw / fdholder，`#include <s6/s6.h>`，链 `-ls6 -lskarnet` | **没有 supervisor 库**；监督器无法以库形式复用 | https://skarnet.org/software/s6/libs6/ | 2.15.1.1 |
| `s6-supervise <servicedir>`：cd 到服务目录 → fork/exec `./run`；run 死 → 跑 `./finish`（默认 5 秒超时后 SIGKILL，可 `./timeout-finish` 配置）；finish 死 → 再起 run；`./down` 文件决定默认状态 | 监督语义就是「run/finish 脚本 + 目录里的控制文件」，不是 API | https://skarnet.org/software/s6/s6-supervise.html | 2.15.1.1 |
| 「两次 `./run` 启动之间至少有 1 秒延迟」；若服务曾 ready 超过 1 秒则立即重启，未 ready 时总是等 1 秒 | s6 自带的最低限度防 busyloop，**不是**崩溃循环上限 | 同上 | 同上 |
| `./finish` 退出 **125** → 发 `O` 事件且**不再重启**服务 | 这是 s6 表达「永久失败」的机制 | 同上 | 同上 |
| `s6-supervise` 总是把子进程放进新 session 并成为 session leader；SIGTERM=bring down + exit，SIGQUIT=立即退，SIGINT=给服务进程组发 SIGINT 后立即退 | 子进程与 supervise 的进程组隔离；信号语义可精确映射到 svcd 的 stop 语义 | 同上 | 同上 |
| `s6-supervise` 是「完全异步状态机」，**不使用 malloc**，每实例非共享内存极小；几十个实例在受限系统上无压力 | 单实例内存开销可忽略，适合嵌入式 | 同上 | 同上 |
| 「You can have a reliable supervision tree even if s6-svscan is not your process 1. The supervision tree just has to be rooted in process 1: your process 1 will have to supervise your s6-svscan process somehow.」并给出 systemd unit（`Type=simple`、`Restart=always`、`RestartSec=1`、`ExecReload=s6-svscanctl -an`、`KillMode=mixed`）与 inittab `SV:123456:respawn:/command/s6-svscanboot -c /dev/console /run/service` | **非 PID 1 复用 s6 是官方支持用法**，且与 svcd 的定位完全同构 | https://skarnet.org/software/s6/s6-svscan-not-1.html | 2.15.1.1 |
| 同页警告：「pipes between services and loggers are maintained by the s6-svscan process; if this process dies, the pipes will be closed and some logs may be lost」 | 引入 s6-svscan 会新增一个「死了就丢日志」的单点，与 svcd 自己的保活叠加 | 同上 | 同上 |
| `s6-permafailon <secs> <deathcount> <events> <prog...>` 读 supervisor 维护的 death tally；匹配则退 125 使服务永久失败；tally 可用 `s6-svdt` 查看、`s6-svdt-clear` 清空 | s6 的崩溃循环上限**要写在 `finish` 脚本里**，不是配置项 —— svcd 必须为每个服务生成这段脚本 | https://skarnet.org/software/s6/s6-permafailon.html | 2.15.1.1 |
| s6-rc 是依赖管理器：`s6-rc-compile` 离线编译服务数据库，`s6-rc-init` 建 live 目录，运行时靠 `s6-rc`；**「s6-rc will only work if there is an active s6-svscan process monitoring a scan directory」**；构建+运行都需要 execline | 要把 s6-rc 塞进 svcd，等于同时引入 s6-svscan + execline + 编译数据库 + 可写 live 目录 —— 成本远高于「复用监督器」 | https://skarnet.org/software/s6-rc/overview.html | s6-rc 0.8.0.2 |
| Alpine `main` 有 s6：apk 167.6 KiB / installed **1006.5 KiB**，依赖 musl、execline、s6-libs、skalibs-libs、busybox-binsh、s6-ipcserver | 全套 s6 二进制在 musl 上约 1 MB installed，已接近/等于整个预算 | https://pkgs.alpinelinux.org/package/edge/main/x86_64/s6 | 2.15.1.1-r0，构建于 2026-10-19 |
| Alpine `s6-static` 描述为「(static library)」，installed 143.6 KiB，依赖 `s6-dev` | 那个 143.6 KiB 是**静态库**，不能当成「静态 s6 二进制只要 144KB」 | https://pkgs.alpinelinux.org/package/edge/main/x86_64/s6-static | 同上 |
| OpenWrt 官方 wiki 的 pkgdata 查不到 `s6` 条目；OpenWrt 官方 init 是 procd | OpenWrt 上复用 s6 需自行打包（**未在官方 feed 穷举验证，见未确认项**） | https://openwrt.org/packages/pkgdata/s6 | 2026-11-12 抓取 |

### systemd：notify 协议、Restart、Watchdog、StartLimit

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `Type=notify`：服务用 `sd_notify()` 或等价调用发 `READY=1` 才算启动完成；`NotifyAccess=` 缺省或 `none` 时会被**强制设为 `main`** | unit 里 `Type=notify` + `NotifyAccess=main` 即可，多余开放没必要 | https://man7.org/linux/man-pages/man5/systemd.service.5.html | man7 镜像，2026-11-12 抓取（页面未标版本号） |
| `NotifyAccess=` 取值 `none`/`main`/`exec`/`all`；`main` 只接受主进程；`exec` 接受主进程与所有 `Exec*=` 派生进程；`all` 接受 cgroup 内全部进程 | 若让插件替 svcd 报就绪，必须 `NotifyAccess=all`，且插件进程在 PID 1 处理消息时必须仍存活否则被丢弃 | 同上 | 同上 |
| `WatchdogSec=`：启动完成后生效，服务须定期发 `WATCHDOG=1`；两次间隔超过配置值则进 failed 并被 `SIGABRT`（或 `WatchdogSignal=`，默认 SIGABRT）终止；该值通过 `WATCHDOG_USEC=` 环境变量传给服务；默认 0 = 关闭 | svcd 只要读 `WATCHDOG_USEC` 就能自适应是否要 ping，无需额外配置 | 同上 | 同上 |
| `Restart=` 取值 `no`/`on-success`/`on-failure`/`on-abnormal`/`on-watchdog`/`on-abort`/`always`，并附「退出原因 × 设置」完整矩阵；clean exit 定义含 exit 0 与 SIGHUP/SIGINT/SIGTERM/SIGPIPE；`systemctl stop` 导致的不重启；`RestartPreventExitStatus=`/`RestartForceExitStatus=` 可覆盖 | 长驻服务推荐 `on-failure`；svcd 想「自杀后不再被拉起」必须用被 `RestartPreventExitStatus=` 覆盖的退出码 | 同上 | 同上 |
| `Restart=` 明确写「service restart is subject to unit start rate limiting configured with `StartLimitIntervalSec=` and `StartLimitBurst=`」 | 重启策略与限流是两个独立旋钮，必须一起设 | 同上 | 同上 |
| `StartLimitIntervalSec=`/`StartLimitBurst=`：在 `interval` 内启动超过 `burst` 次后**不允许再启动**；interval 可为 0 关闭、可为 `infinity`；**对所有启动方式生效（含手动）**；达到上限后手动/定时器/套接字仍可稍后拉起，之后重启逻辑恢复；`systemctl reset-failed` 清零计数；**限流在条件检查之后执行**，条件失败不计数；单元被 GC 时计数清零 | 「5 次/10 秒」是默认的崩溃循环刹车；svcd 自身重启也计入 | https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html | systemd 262 |
| 默认值：`DefaultStartLimitIntervalSec=10s`、`DefaultStartLimitBurst=5`、`DefaultRestartSec=100ms`、`DefaultTimeoutStartSec=90s` | 不写这些字段时的实际行为基线 | https://www.freedesktop.org/software/systemd/man/latest/systemd-system.conf.html | systemd 262 |
| `sd_notify()` 协议：向 `$NOTIFY_SOCKET`（AF_UNIX，路径或 `@` 抽象命名空间，也支持 `vsock:`）发**一条 datagram**，内容是换行分隔的「类环境块」赋值；未知字段被忽略，私有扩展建议 `X_` 前缀 | 协议极简，几十行 C 可实现 | https://man.archlinux.org/man/sd_notify.3.en | systemd 262.2 |
| 已定义字段：`READY=1`、`RELOADING=1`+`MONOTONIC_USEC=`、`STOPPING=1`、`STATUS=`、`ERRNO=`、`BUSERROR=`、`MAINPID=`、`WATCHDOG=1`、`WATCHDOG=trigger`、`WATCHDOG_USEC=`、`EXTEND_TIMEOUT_USEC=`、`RESTART_RESET=1`、`FDSTORE=1`/`FDNAME=`、`BARRIER=1` | svcd 最少实现 `READY=1`/`STOPPING=1`/`STATUS=`/`WATCHDOG=1` 就够用 | 同上 | 同上 |
| 手册内含 **MIT-0 的无依赖 C 参考实现**，并注明协议受 PORTABILITY_AND_STABILITY 保证 | **不必链接 libsystemd**；对静态 musl 目标是决定性的 | 同上 | 同上 |
| `KillMode=` 默认 `control-group`：单元停止时 cgroup 内所有剩余进程都被杀；`mixed` 只对主进程发 SIGTERM、对其余进程发 SIGKILL；`process`/`none` 被明确不推荐 | systemd 上「插件随 svcd 一起死」是**白拿**的，不用 svcd 自己管 | https://www.freedesktop.org/software/systemd/man/latest/systemd.kill.html | systemd 262 |
| `KillSignal=` 默认 SIGTERM，随后按 `TimeoutStopSec=` 到期发 `SIGKILL`（`FinalKillSignal=` 默认 SIGKILL） | stop 的时序契约：svcd 有 `TimeoutStopSec` 那么长的窗口做优雅退出 | 同上 | 同上 |

### launchd：plist 键与 launchctl 现代用法

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `KeepAlive` 可为 bool 或条件字典（`SuccessfulExit`/`Crashed`/`PathState`/`OtherJobEnabled`，多键之间是 **OR**）；默认 false 即纯按需启动；**「The use of this key implicitly implies RunAtLoad」** | 想要「常驻 + 崩了就起」用 `KeepAlive=true`，同时自动获得 `RunAtLoad` | https://keith.github.io/xcode-man-pages/launchd.plist.5.html | Darwin，2019-07-30 |
| `ThrottleInterval`：「by default, jobs will not be spawned more than once every 10 seconds」 | launchd 的崩溃循环刹车就是这个 10 秒；想要别的节奏只能覆盖它 | 同上 | 同上 |
| `RunAtLoad`：加载时启动一次，默认 false；原文「This key should be avoided, as speculative job launches have an adverse effect on system-boot and user-login scenarios」 | 靠 `KeepAlive` 间接获得即可，不要显式写 `RunAtLoad` | 同上 | 同上 |
| `ProcessType`：`Background`/`Standard`/`Adaptive`/`Interactive`，决定系统施加的资源限制；不设则「apply light resource limits」；`Standard` 等价于不设 | 常驻守护进程建议 `Background`，避免抢占前台体验 | 同上 | 同上 |
| `AbandonProcessGroup`：「When a job dies, launchd kills any remaining processes with the same process group ID as the job. Setting this key to true disables that behavior.」 | **macOS 上「子进程随管理器一起死」是靠 process group**，不是 subreaper；想保留子进程才设 true | 同上 | 同上 |
| `ExitTimeOut`：SIGTERM 到 SIGKILL 之间等待秒数；0 被解释为无穷且「should not be used」 | svcd 在 macOS 的优雅退出窗口由它决定 | 同上 | 同上 |
| `EnableTransactions`：活跃进程 STOP 时先 SIGTERM、超时后 SIGKILL；**非活跃进程直接 SIGKILL** | 若 svcd 想拿到优雅退出，必须保持「活跃」状态 | 同上 | 同上 |
| launchd 托管的进程 **MUST NOT** `daemon(3)`、也不得 fork 后父进程 exit/_exit；不应 `setsid`、不应自己重定向 stdio；应处理 SIGTERM | svcd 在 macOS 必须前台运行、不 daemonize —— 与 `--foreground` 语义一致 | 同上 | 同上 |
| `launchctl` 现代子命令是 `bootstrap` / `bootout` / `enable` / `disable` / `kickstart` / `print`；`load`/`unload` 被标为 **LEGACY**，且注明「Recommended alternative subcommands: bootstrap \| bootout \| enable \| disable」 | `install-service` 该用 `bootstrap` + `enable`，`uninstall-service` 用 `bootout` | https://keith.github.io/xcode-man-pages/launchctl.1.html | Darwin，2014-10-01 |
| target 形式：`system/<svc>`、`user/<uid>/<svc>`、`gui/<uid>/<svc>`、`login/<asid>/<svc>`、`pid/<pid>/<svc>`；system domain 需 root 才能修改 | 系统级守护进程 → `system/` | 同上 | 同上 |
| legacy `load`/`unload` 因历史 bug「will only return a non-zero exit code due to improper usage. Otherwise, zero is always returned」 | 用 legacy 子命令做错误检测是不可能的，必须用 bootstrap/bootout | 同上 | 同上 |
| Apple 归档文档：daemon 启动后太快退出「launchd may think it has crashed」，并建议「do not shut down for at least 10 seconds after launch」；反复如此会被挂起不再拉起（"respawning too fast"） | 与 `ThrottleInterval` 一致的第二手确认 | https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html | 归档，更新于 2016-09-13 |

### Windows SCM 与 Job Object

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `main` 通过 `StartServiceCtrlDispatcher(dispatch_table)` 接入 SCM，该调用在进程内所有服务进入 `SERVICE_STOPPED` 前不返回；主线程应在启动后 **30 秒内**调用 | service 模式必须尽早在主线程接入 dispatcher | https://learn.microsoft.com/en-us/windows/win32/api/winsvc/nf-winsvc-startservicectrldispatchera | 2023-02-09 |
| 控制台运行时该调用失败并置 `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT`；微软建议「structure it such that service-specific code is not called when this error is returned」 | `svcd run --foreground` 与 service 模式的**官方标准探测法** | 同上 | 同上 |
| 每个服务有 `Handler`/`HandlerEx` 回调，由 `RegisterServiceCtrlHandler(Ex)` 注册；**必须处理 `SERVICE_CONTROL_INTERROGATE`**；若接受 `SERVICE_CONTROL_STOP` 就必须停下（进 `STOP_PENDING` 或 `STOPPED`）；处理后若状态变化才需 `SetServiceStatus` | stop/pause/关机全部经这一个回调 | https://learn.microsoft.com/en-us/windows/win32/services/service-control-handler-function | 2021-01-07 |
| **控制处理器必须在 30 秒内返回**，否则 SCM 报错；耗时清理应另起线程、先报 `SERVICE_STOP_PENDING` 再返回 | svcd 的 stop 路径不能阻塞 handler | 同上 | 同上 |
| 关机顺序：先给声明 `SERVICE_ACCEPT_PRESHUTDOWN` 的服务发 `SERVICE_CONTROL_PRESHUTDOWN`（等它停或预设超时），再按安装顺序发 `SERVICE_CONTROL_SHUTDOWN`；默认约 **20 秒**清理时间；Services 面板停机上限 **125 秒**；重启时上限取注册表 `WaitToKillServiceTimeout`；SCM 处理函数优先级 0x1E0（`SetProcessShutdownParameters`） | 嵌入式/网关场景必须假设「关机只给 ~20 秒」 | 同上 | 同上 |
| 关机时 **SCM 默认不考虑依赖关系**，按枚举顺序发 SHUTDOWN，所以依赖可能已先停 | Windows 上不能指望「依赖先停」；svcd 的依赖顺序得自己保证 | 同上 | 同上 |
| `SERVICE_STATUS`：`dwCurrentState`（START_PENDING/RUNNING/STOP_PENDING/STOPPED/PAUSED…）、`dwControlsAccepted`（STOP/SHUTDOWN/PRESHUTDOWN/PARAMCHANGE/PAUSE_CONTINUE…）、`dwWin32ExitCode`（置 `ERROR_SERVICE_SPECIFIC_ERROR` 时才用 `dwServiceSpecificExitCode`）、`dwCheckPoint`、`dwWaitHint` | 服务退出码有两条通道：Win32 错误码或服务自定义码 | https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_status | 2021-04-02 |
| 「If the amount of time specified by `dwWaitHint` passes, and `dwCheckPoint` has not been incremented or `dwCurrentState` has not changed, the SCM … can assume that an error has occurred and the service should be stopped」 | pending 状态必须持续 checkpoint，否则 SCM 认定卡死 | 同上 | 同上 |
| `SERVICE_FAILURE_ACTIONS`：「A service is considered failed when it terminates without reporting a status of `SERVICE_STOPPED`」；`dwResetPeriod` 到期清零失败计数；第 N 次失败执行 `lpsaActions[N-1]`，超过数组长度**重复最后一个动作** | **Windows 的崩溃循环上限在 SCM 侧、安装时配置**，服务进程内改不了 | https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actionsa | 2024-02-22 |
| Job Object：`CreateJobObject` + `AssignProcessToJobObject`；关联后不可解除；**默认子进程经 `CreateProcess` 会自动继承 job 归属**；`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 时「closing the last job object handle terminates all associated processes and then destroys the job object itself」；嵌套 job 从 Win8/2012 起支持，嵌套时父 job 的 KILL_ON_JOB_CLOSE 也会杀掉子 job 内进程 | 「插件进程随 svcd 一起死」在 Windows 上的正解：svcd 建一个带该 limit 的 job 并把插件放进去，svcd 进程一消失句柄关闭 → 插件全灭 | https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects | 2025-07-14 |
| `windows-service` crate 0.8.1，MIT OR Apache-2.0，依赖 bitflags/widestring/windows-sys 0.61；提供 `define_windows_service!`、`service_dispatcher::start`、`service_control_handler::register` + `ServiceStatusHandle::set_service_status`、`service_manager`（安装/删除/启停）、`ServiceStatus{current_state, controls_accepted, exit_code, checkpoint, wait_hint}`、`ServiceExitCode::Win32/ServiceSpecific` | Rust 侧不必手写 FFI；`wait_hint`/`checkpoint` 有类型封装 | https://docs.rs/windows-service/latest/windows_service/ | 0.8.1，发布 2026-09-24 |
| 该 crate 文档：「The system will attempt to kill a service that is not able to transition to `Running` state before the proposed `wait_hint` expired.」 | 启动慢必须边推进边 checkpoint | 同上 | 同上 |

### BusyBox init 与 procd

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| BusyBox `init` 源码：`if (getpid() != 1 && (!ENABLE_LINUXRC \|\| applet_name[0] != 'l')) bb_simple_error_msg_and_die("must be run as PID 1")` | BusyBox init **只能当 PID 1**（或 linuxrc），不可能作为 svcd 的非 PID 1 监督器 | https://sources.debian.org/data/main/b/busybox/1%3A1.37.0-6/init/init.c | BusyBox 1.37.0-6（Debian） |
| inittab 格式 `id:runlevels:action:process`；action 集合 `sysinit wait once respawn askfirst ctrlaltdel shutdown restart`；**runlevels 字段被完全忽略**；id 字段是控制 tty（拼到 `/dev/` 后面） | 接 BusyBox 只需要一行 `::respawn:/usr/sbin/svcd run --foreground` | 同上（源码内 `init_notes_usage`） | 同上 |
| respawn 语义：「When a process started with a 'respawn' action exits, init automatically restarts it. **Unlike sysvinit, BusyBox init does not stop processes from respawning out of control.**」主循环里有 `sleep1()` | **BusyBox 没有任何崩溃循环上限**，最坏约 1 次/秒无限重启 | 同上 | 同上 |
| 子进程侧 `setsid()`（成为 session/pgid leader）；init 用 `waitpid(-1, WNOHANG)` 循环回收所有子进程 | BusyBox init 作为 PID 1 会回收僵尸，svcd 不需要替它做 | 同上 | 同上 |
| 关机：`run_actions(SHUTDOWN)` 后 `kill(-1, SIGTERM)` → `sync()` → `kill(-1, SIGKILL)` | 没有任何 per-service 顺序；svcd 的依赖顺序在 BusyBox 上必须自己实现，且关机时可能被直接 SIGKILL | 同上 | 同上 |
| `respawn` 条目每轮循环只在 `a->pid == 0` 时重启；若条目 tty 不可读写则跳过 | tty 相关条目在无 tty 环境会被静默跳过，`::respawn:` 留空 id 才安全 | 同上 | 同上 |
| procd init 脚本：`#!/bin/sh /etc/rc.common` + `START=`/`STOP=` + `USE_PROCD=1`；`start_service()` 里 `procd_open_instance` / `procd_set_param` / `procd_close_instance`；服务必须以**前台**方式运行 | OpenWrt 上 svcd 也要「前台运行 + 由 procd 托管」 | https://openwrt.org/docs/guide-developer/procd-init-scripts | 页面最后修改 2025-04-23 |
| `respawn` = `<threshold> <timeout> <retry>`：threshold 秒窗口内重启达 retry 次即判 crashed、不再重启，直到显式 restart；**retry=0 表示无限重试**；默认 `3600 5 5` | procd 自带崩溃循环刹车，且是**唯一**能配「无限重试」的默认语义 | 同上 | 同上 |
| `term_timeout` 默认 5 秒，之后 `KILL` | svcd 在 OpenWrt 上的优雅退出窗口默认只有 5 秒 | 同上 | 同上 |
| 「procd only tracks the main process」——用管道派生的子进程在 `stop` 后仍存活；「The program needs to keep track of its sub-processes and terminate them properly when it receives a SIGTERM」 | **procd 不替 svcd 清理插件**；这条 gotcha 正是 svcd 存在的理由 | 同上 | 同上 |
| 其他可用参数：`env`/`limits`(ulimit)/`file`/`netdev`/`data`（变更触发重启）、`stdout`/`stderr`（转 logd）、`pidfile`、`user`、`nice`、`reload_signal`、`seccomp`/`capabilities`/`no_new_privs`（ujail） | 对接面很宽，`file`/`netdev` 可以做成 svcd 的依赖/重载触发器 | 同上 | 同上 |

### 非 PID 1 的子进程回收

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `prctl(PR_SET_CHILD_SUBREAPER, set)`：subreaper 对后代扮演 init 角色；孤儿被 reparent 到「最近的仍存活的祖先 subreaper」，终止时向 subreaper 发 SIGCHLD 并可 `wait()`；**该属性不被 fork 出的子进程继承**，但**跨 execve 保留** | Linux 上非 PID 1 监督器要拿到孙进程退出状态，唯一正解就是在自己身上设 subreaper | https://man7.org/linux/man-pages/man2/PR_SET_CHILD_SUBREAPER.2const.html | Linux man-pages 6.19（2026-02-08），内核 3.4+ |
| 同页 STANDARDS 字段仅「Linux」；并注明 systemd 等 init 框架「employ a subreaper process for similar reasons」 | **没有 POSIX / Darwin 等价物**；macOS 侧不存在可移植替代 | 同上 | 同上 |
| 不设 subreaper 时：孤儿孙进程被 reparent 给 PID 1，svcd 既收不到 SIGCHLD 也无法 `wait()`，只能靠轮询/进程组间接感知 | 这是自研监督器最容易被忽略的正确性缺口 | 推断（基于上面两条 + POSIX 语义） | — |
| macOS 的实际机制是进程组：launchd 在 job 死亡时杀掉同 pgid 的剩余进程（`AbandonProcessGroup=true` 可关闭） | macOS 上「插件随 svcd 死」用「插件进独立进程组 + svcd 退出前 kill(-pgid)」实现；宁可留孤儿也要显式选择 | https://keith.github.io/xcode-man-pages/launchd.plist.5.html | Darwin，2019-07-30 |
| Windows 没有僵尸概念：`WaitForSingleObject` 拿进程句柄退出码；进程树归属用 Job Object，`KILL_ON_JOB_CLOSE` 在最后一个句柄关闭时杀掉全部关联进程 | Windows 的子进程生命周期管理是「句柄 + job」模型，不需要回收器 | https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects | 2025-07-14 |

### 崩溃循环防护对比（同一件事，五个不同落点）

| 平台 | 机制 | 落点 | 能否配置成「无限重试」 |
| --- | --- | --- | --- |
| systemd | `Restart=` × `StartLimitBurst=5` / `StartLimitIntervalSec=10s`，`reset-failed` 清零 | 管理器侧（svcd 不用写） | 能（interval=0 关闭限流） |
| launchd | `ThrottleInterval` 默认 10 秒一次；「respawning too fast」会被挂起 | 管理器侧 | 节流而非上限，表现为「永不放弃但很慢」 |
| Windows SCM | `SERVICE_FAILURE_ACTIONS`（restart/reboot/run command）+ `dwResetPeriod`；服务「未报 STOPPED 就终止」即算失败 | 管理器侧，**安装时配置** | 能（超过数组长度重复最后一个动作） |
| procd | `respawn <threshold> <timeout> <retry>`，默认 `3600 5 5` | 管理器侧（init 脚本参数） | 能（retry=0） |
| BusyBox init | 无 | 无 | 天然无限（且无限快） |
| s6 | 1 秒最小间隔 + `finish` 退 125 = 永久失败 + `s6-permafailon` 按 death tally 判永久失败 | **svcd 生成的 finish 脚本侧** | 能（不写 permafailon 即可） |
| 自研监督器 | 自己实现退避 + 失败上限 + 复位窗口 | svcd 自己 | 自己定 |

## 对技术选型的影响

### 1. 不要复用 s6 作为监督内核（确定性：高）

理由按成本排序：

| 维度 | 自研监督器 | 复用 s6 |
| --- | --- | --- |
| 交付形态 | 一个静态二进制 | 额外一组二进制（`s6-supervise`/`s6-svscan`/`s6-svc`…）+ 每服务 `run`/`finish` 脚本；`libs6` 无 supervisor 接口，**无法链接** |
| 体积 | 监督逻辑本身极小（异步状态机 + fork/exec + waitpid + 退避） | Alpine 实测整包 installed 1006.5 KiB（含 60+ applet、execline、skalibs 动态库）；最小子集需自行裁剪构建，**未实测** |
| 平台覆盖 | 一套语义，三平台各自落地（Linux subreaper / macOS pgid / Windows job） | **Windows 完全不适用**（POSIX-only）；macOS 支持程度未在本文核实。等于只在 Linux 上省事，另两平台仍要自研 |
| 语义融合 | 依赖顺序、重启策略、就绪通知、退避都在自己的配置模型里 | 两套语义叠加：s6 的重启（1 秒下限、finish 125）与 svcd 自己的策略要缝起来；崩溃循环上限要靠生成 `finish` 脚本调 `s6-permafailon` |
| 日志 | 自己掌控 | s6 官方警告 s6-svscan 一死管道断、日志丢 |
| 依赖/许可证 | 无 | ISC（友好）+ skalibs/execline（同为 ISC，但增加构建与供应链面） |
| 可测试性 | 可直接对状态机做单测 | 只能黑盒测 s6 二进制行为，或依赖 `s6-svdt`/`s6-svstat` 解析 |
| 团队认知负担 | 一套代码 | 需要理解服务目录、`down`/`notification-fd`/`timeout-finish`、execline、`s6-svc` 信号矩阵 |

**结论：s6 是「Linux 上省 2 周、其余平台白干、且体积和语义都变复杂」的交易。对 ≤1MB 三平台目标，自研监督器的总成本更低，而 s6 只在单平台 PoC 上是划算的。**

唯一值得借的不是代码，是**设计**：服务目录式的 run/finish 分离、`finish` 退出码 125 = 永久失败、「ready 后立即重启 / 未 ready 至少等 1 秒」这条二分支退避规则——这三条直接照抄进 svcd 的退避策略，成本为零。

### 2. 自研监督器必须内置的、四平台共用的最小集（确定性：高）

1. **退避**：照 s6——曾 ready 且运行超过 N 秒则立即重启；未 ready 则至少等 1 秒，并按次数指数退避。上限后进入「permanent failure」终态，需显式 `svcd restart` 才复位（对齐 s6 的 125 / procd 的 crashed / systemd 的 failed）。
2. **子进程回收**：Linux 上进程启动即 `prctl(PR_SET_CHILD_SUBREAPER, 1)`；macOS 用独立进程组 + `kill(-pgid)`；Windows 用 `KILL_ON_JOB_CLOSE` 的 Job Object。三者都是「一次性的、可测的」工作，不是框架。
3. **就绪协议**：读 `$NOTIFY_SOCKET` 就发 `READY=1`（零依赖 C 参考实现可移植），否则退化为「exec 成功即 ready」。**不要**链接 libsystemd。
4. **停止时序**：`TimeoutStopSec`/`term_timeout`/`ExitTimeOut`/`dwWaitHint` 都是外部给的窗口，svcd 只需要接受一个 `--stop-timeout` 并对齐到 unit 里的值。

### 3. `svcd install-service` 该写什么（确定性：高，字段来自上面引用的一手文档）

**systemd**（`/etc/systemd/system/svcd.service`）：

```ini
[Unit]
Description=svcd service manager
# 崩溃循环刹车：默认就是 5 次/10 秒，显式写出便于审计
StartLimitIntervalSec=10s
StartLimitBurst=5

[Service]
Type=notify                      # svcd 实现 READY=1 / STOPPING=1 / STATUS=
NotifyAccess=main                # 只信主进程，别开 all
ExecStart=/usr/sbin/svcd run --foreground
ExecReload=/usr/sbin/svcd reload
Restart=on-failure               # 官方推荐长驻服务取值
RestartSec=1
TimeoutStopSec=30                # svcd 的 --stop-timeout 要 ≤ 此值
WatchdogSec=0                    # 若实现 WATCHDOG=1 再开，并用 WATCHDOG_USEC 自适应
KillMode=control-group           # 默认值；插件随 svcd 一起死，不必自己管
# 若希望 svcd 自杀后不被拉起：配合 RestartPreventExitStatus=<专用码>

[Install]
WantedBy=multi-user.target
```

要点：`Type=notify` 是唯一能让 `systemctl start svcd` 等到「svcd 真就绪」的取值（`Type=exec` 只等到 execve 成功）；`KillMode=control-group` 让插件清理白拿，但**不能**因此省掉 svcd 自己的顺序化停止（依赖顺序仍需 svcd 保证）。

**launchd**（`/Library/LaunchDaemons/<label>.plist`）：

```xml
<key>KeepAlive</key><true/>        <!-- 隐含 RunAtLoad，别另写 RunAtLoad -->
<key>ThrottleInterval</key><integer>10</integer>  <!-- 默认值，写明避免误解 -->
<key>ProcessType</key><string>Background</string>
<key>ExitTimeOut</key><integer>30</integer>
<!-- 不要写 AbandonProcessGroup，保留 launchd 杀掉同 pgid 进程的默认行为 -->
```

安装：`launchctl bootstrap system /Library/LaunchDaemons/<label>.plist` + `launchctl enable system/<label>`；卸载：`launchctl bootout system/<label>`。**不要用 `load`/`unload`**（legacy，且永远返回 0）。

**Windows SCM**：安装时 `CreateService` + `ChangeServiceConfig2(SERVICE_CONFIG_FAILURE_ACTIONS)`（`SC_ACTION` 数组 + `dwResetPeriod`）；启动时 `StartServiceCtrlDispatcher`，若返回 `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` 则走前台模式；`ServiceMain` 里注册 `HandlerEx`、先报 `START_PENDING` + `wait_hint`/`checkpoint` 推进、就绪后报 `RUNNING`；停止时另起线程清理、报 `STOP_PENDING` 后从 handler 返回（30 秒硬限）。插件生命周期用带 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object。

**BusyBox init**（`/etc/inittab`）：一行 `::respawn:/usr/sbin/svcd run --foreground`。**必须**假设没有任何崩溃循环保护，退避只能靠 svcd 自己。

**procd**（`/etc/init.d/svcd`）：

```sh
#!/bin/sh /etc/rc.common
START=90
USE_PROCD=1
start_service() {
    procd_open_instance
    procd_set_param command /usr/sbin/svcd run --foreground
    procd_set_param respawn 3600 5 5    # 默认值；0 表示无限重试
    procd_set_param term_timeout 30     # 默认 5 秒太短
    procd_set_param stdout 1
    procd_set_param stderr 1
    procd_close_instance
}
```

注意 procd 只跟踪主进程，**插件清理必须 svcd 自己做**（这正好是 svcd 的价值点，不要指望 procd）。

### 4. 体积判断（确定性：中——基于包元数据推理，未实测）

- 「s6 式监督」的逻辑面积极小：单实例异步状态机、无 malloc、每服务一个 fork/exec + waitpid 循环。**可以**放进 1MB 静态 musl 二进制，而且只占其中很小一部分（推断）。
- 但「vendor 整个 s6」不行：Alpine 上 `s6` 包 installed 1006.5 KiB 就已经吃掉全部预算，且不含 execline 的运行期成本。要压到 1MB 以内必须自己裁剪构建（关闭 execline、只留 3 个 applet），而那已经不是「复用」而是「fork 维护」。
- 若要给出确定答案，唯一诚实的做法是实测：musl-gcc `-Os -static` 只编 `s6-supervise` + `s6-svscan` + `s6-svc`，量 `size` 输出。**本文未做，列为未确认项。**

## 未确认项

1. **s6 最小二进制子集的确切静态体积**：未实测。已知只有 Alpine 整包数据（installed 1006.5 KiB、apk 167.6 KiB）与 `s6-static` 静态库 143.6 KiB。`s6-supervise`/`s6-svscan`/`s6-svc` 三件套在 `musl -Os -static` 下的字节数未知。
2. **s6 在 macOS 的上游支持程度**：本文只确认 s6 是 POSIX 工具、依赖 skalibs；未核实 skalibs/s6 是否官方支持 Darwin，也未核实 Homebrew 之外的分发路径。这直接影响「复用 s6 能否覆盖 macOS」——目前只能确定**不能覆盖 Windows**。
3. **OpenWrt 官方 feed 是否提供 s6**：`https://openwrt.org/packages/pkgdata/s6` 返回「This topic does not exist yet」，但这是 wiki 生成页，未穷举各版本 feed 与 packages 仓库。结论「OpenWrt 上需自行打包」属于**中等置信推断**。
4. **不设 subreaper 时孤儿孙进程行为的精确定义**：本文按 POSIX「孤儿 reparent 给 PID 1（或最近 subreaper）」推断 svcd 会失去退出状态，未在真实内核上验证 svcd 场景（尤其双重 fork 的 daemon）。
5. **Windows：SCM 停止流程与 `KILL_ON_JOB_CLOSE` 的交互顺序**：推断 svcd 进程退出即句柄关闭即插件被杀，但未实测「SCM 先杀 svcd 再等插件」还是相反，也未实测 job 内进程在 `SERVICE_CONTROL_PRESHUTDOWN` 阶段的行为。
6. **systemd `NotifyAccess=all` 与插件代报就绪**：协议上可行（cgroup 内进程都算），但「插件进程发送后立即退出会被丢弃」这一点使得该用法在插件形态下天然脆弱。是否值得支持，未做设计验证。
7. **launchd `KeepAlive` 为字典且多键 OR 时，与 `ThrottleInterval` 的叠加细节**：文档给出 OR 语义与 10 秒节流，但「被节流期间条件仍成立」时的具体重试时刻未在文档中量化。
