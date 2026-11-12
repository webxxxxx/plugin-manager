# 插件进程的身份与权限边界：Linux / Windows / macOS 的 OS 机制与可行性

调研日期：2026-11-12。问题：svcd（root / Administrator / LocalSystem 的单一守护进程）要 spawn
第三方插件进程，**这些子进程应该以什么身份、受什么边界约束**？本笔记只提供 OS 机制事实与
可行性判断；产品取舍由维护者拍板。

**取证方式说明**：全部为一手来源（man7 man-pages 6.19、Linux 内核源码 git.kernel.org、
Microsoft Learn、Apple 官方手册页镜像与 XNU 头文件、systemd 官方文档）。**没有做任何本机实测**：
没有在三个平台上跑 setuid / CreateProcessAsUser / launchd 的验证程序，没有跑 `ls -l /proc/<pid>`、
没有试过 AppContainer 跑原生 exe。凡属推断都标了【推断】，查不到的进「未确认项」。
本轮部分 Microsoft Learn 页面抓取超时/404（Service SID、AppContainer legacy apps），
相关结论已降级标注。

## 结论（先给答案）

1. **Linux 上「root 守护进程 spawn 后降权」是成熟路径，成本最低**：`fork()` 后在子进程里
   `setgroups(0,NULL) → setgid() → setuid()`，再 `execve()`。父进程始终是 root，
   **wait / signal / kill 完全不受子进程 uid 变化影响**（wait 基于父子关系；kill 走 CAP_KILL）。
   【文档】kill(2) 的权限规则、wait(2) 的语义都写明。
2. **Linux 上「每插件一个系统用户」可落地，但没有 systemd 那样的现成设施**：
   `useradd -r` 在 `SYS_UID_MIN..SYS_UID_MAX`（默认 101–999）里取号、默认不建 home；
   OpenWrt/BusyBox 环境要么有 `adduser -S`，要么得自己写 `/etc/passwd`。
   **UID 回收复用是真实风险**（systemd 自己文档里就警告 DynamicUser 的 UID 会回收）。
3. **capabilities 能给"非 root uid 的插件"只发一两项能力，但路径有两条、各有坑**：
   文件能力（`security.capability` xattr，需 CAP_SETFCAP，**对 shebang 脚本无效**）vs
   父进程 ambient 能力（`PR_CAP_AMBIENT_RAISE`，但 **exec 一个带 file caps 或 setuid 位的程序会清空 ambient**）。
   两条都不覆盖"插件自己再 spawn 子进程"的所有情况，也不是"能力沙箱"。
4. **Linux 的 UDS 对端身份够用**：`SO_PEERCRED` 返回 connect 时的 pid/uid/gid；
   每插件一个路径 socket + 目录 0700 + socket 属主即可。**抽象命名空间（abstract）没有文件权限**，
   用 abstract socket 就必须靠 `SO_PASSCRED`/`SCM_CREDENTIALS` 自己校验，不能靠文件系统。
5. **"共享一个非特权账号"买到的是"碰不到系统"，不是"插件之间互不可见"**：同 uid 可以互相
   `kill()`（kill(2) 明确：real/effective uid 等于目标 real/saved uid 即可）；同 uid 能读
   `/proc/<pid>/environ`、`cmdline`（这些走 PTRACE_MODE_READ，Yama 默认不管）；
   `/proc/<pid>/mem` 与 ptrace 受 Yama `ptrace_scope=1` 约束（默认只允许祖先 attach），
   但**创建 user namespace 会绕过 Yama**（ptrace(2) 原文写明）。所以"同 uid 互相隔离"只是部分成立。
6. **Windows 上从 LocalSystem spawn 成别的身份有两套 API，特权要求不同**：
   `CreateProcessAsUser` 需要 `SeIncreaseQuotaPrivilege`，token 非 assignable 时还要
   `SeAssignPrimaryTokenPrivilege`；**如果传的是自己主令牌的受限版本，则不需要
   SeAssignPrimaryToken**（CreateRestrictedToken 文档明写）。`CreateProcessWithTokenW` 只需要
   `SeImpersonatePrivilege`。服务进程（LocalSystem）通常两者都有，所以这条路在 Windows 上是通的。
7. **Windows 上"每插件一个本地用户"可行但最贵**：`NetUserAdd` 需要管理员（失败码含
   `ERROR_ACCESS_DENIED`），且本地账号要用密码做登录凭据——等于 svcd 必须托管一批密码
   (或改用 LSA secret)，还要处理安装期管理员、域/GPO 冲突。
8. **AppContainer 是 Windows 上更彻底、也更贴合"第三方代码"的边界**：文档写明它做
   凭据/设备/文件/网络/进程/窗口六个维度的隔离，网络按 capability 授予，
   但**代价是每个插件的数据目录/管道都要按 AppContainer SID 重新 ACL**，且文档注记
   AppContainer 内的命名管道要用 `\\.\pipe\LOCAL\` 形式（见证据表 W8）——
   这直接冲击 svcd 现有的"stdio NDJSON 默认 + 可选命名管道"设计。
9. **Windows 上杀进程树用 job object**：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 在最后一个
   句柄关闭时终止全部关联进程，Win8+ 支持嵌套 job。这比"逐个 TerminateProcess"可靠。
10. **macOS 是三者中最弱的**：LaunchDaemon 默认 root；launchd 的 `UserName`/`GroupName`
    是**整个 job 级别**、且只在系统域有效——做不到"一个 svcd 进程里给每个插件不同身份"。
    `posix_spawn` 在 macOS 上没有"设置任意 uid"的属性【推断，基于手册页无 SETUID 属性】，
    要降权只能 fork+setuid 再 exec。`sandbox-exec` 手册页已标 DEPRECATED，
    官方指向 App Sandbox——而 App Sandbox 面向 App 打包，不是"任意第三方二进制"。
11. **macOS 的 UDS 对端身份有等价物**：`LOCAL_PEERCRED`（0x001，`SOL_LOCAL`=0），
    另有 `LOCAL_PEERPID` / `LOCAL_PEEREPID` / `LOCAL_PEERUUID` / `LOCAL_PEERTOKEN`（audit token，
    来自 XNU `bsd/sys/un.h`）。所以"只允许某 uid 的插件连某 socket"在 macOS 上可判。
12. **四种模型的可行性排序（不是产品建议）**：全 root = 三平台零成本；
    共享一个非特权账号 = Linux/macOS 便宜、Windows 需要一个账号；每插件独立身份 =
    Linux 中等（自管 uid 生命周期）、Windows 中高（账号或 AppContainer+ACL）、macOS 最高（无沙箱兜底）；
    两级（system=root，user=共享/独立）= 是唯一能在三平台都"今天就能做"的形状。
13. **对 svcd 自身操作的硬约束**：一旦插件以别的 uid 跑，svcd 就要处理
    "root 写 plugin 用户拥有的目录"（升级/回滚/日志/配置）、"数据目录 0700 + 属主"、
    "卸载时删 uid 与文件"、"uid 复用导致旧数据被新插件读到"（systemd 文档明确此风险）。
    这些不是安全问题，是**产品必须提前定的运维契约**。

## 证据

### A. Linux

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `setuid(2)`：调用者持 CAP_SETUID 时，real/effective/saved 三个 uid 一起改；root 调 `setuid()` 后"impossible for the program to regain root privileges"，要来回切只能用 `seteuid()` | 永久降权用 `setuid`/`setresuid`，不要用 `seteuid` 做"临时降权" | https://man7.org/linux/man-pages/man2/setuid.2.html | man-pages 6.19（2026-02-08） |
| `setresuid(2)`：三个 id 可分别设，`-1` 表示不改；同时总把 fsuid 设成新 euid | 想保留 saved-uid 做审计/回收，得用它 | https://man7.org/linux/man-pages/man2/setresuid.2.html | man-pages 6.19 |
| 补充组要在改 uid 之前清：`setgroups(2)` 需要 CAP_SETGID；改成非 root 后就失去该能力 | 【推断】正确顺序是 `setgroups(0,NULL) → setgid → setuid`，顺序反了会 EPERM 或留下 root 组 | https://man7.org/linux/man-pages/man2/setgroups.2.html | man-pages 6.19 |
| UID 变化对能力的影响：real/effective/saved 曾有一个是 0、改完全非 0 ⇒ permitted/effective/ambient 全清；euid 0→非0 清 effective；fsuid 0→非0 清一组文件相关能力 | 降权即"净身"；要保留能力必须显式用 SECBIT_KEEP_CAPS 或 ambient | https://man7.org/linux/man-pages/man7/capabilities.7.html | man-pages 6.19 |
| `SECBIT_KEEP_CAPS`：允许全 UID 变非 0 时保留 permitted，但 effective 仍被清；**execve 时该位总被清** | 只能当"降权到 exec 之间"的过渡，不能跨 exec | 同上 | man-pages 6.19 |
| ambient 不变量：一项能力必须同时在 permitted 与 inheritable 才能是 ambient；execve 一个**带 file capabilities 或 setuid/setgid 位**的程序会清空 ambient | 给插件 ambient 能力时，插件目录里任何 setuid 二进制都会把能力抹掉（不是提权，是丢能力） | 同上（Ambient capabilities 节） | man-pages 6.19 |
| 内核源码：`cap_bprm_creds_from_file()` 里 `/* File caps or setid cancels ambient. */ if (has_fcap || id_changed) cap_clear(new->cap_ambient);` | 与上条一致，实现级确认 | https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/security/commoncap.c | torvalds/master @ 2026-11 |
| 文件能力存在 `security.capability` xattr，写入需 CAP_SETFCAP；root 执行程序时 file caps 被"当作全 1" | 安装期由 svcd(root) `setcap` 是唯一干净方式；插件自己改不了 | https://man7.org/linux/man-pages/man7/capabilities.7.html | man-pages 6.19 |
| 脚本的凭据来自**解释器**：`exec_binprm()` 把 `bprm->file` 换成 `bprm->interpreter` 后重跑 handler，`begin_new_exec()` 里"Once we are committed compute the creds"才算 | 【推断，源码结构明确】shebang 脚本上的 setuid 位与 file caps 不生效；给"脚本插件"发能力必须发给解释器（如 node/python 二进制），那是全局副作用 | https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/fs/exec.c | torvalds/master @ 2026-11 |
| `execve(2)`：no_new_privs 置位、文件系统 nosuid、或被 ptrace 时，**setuid/setgid 位与文件能力都被忽略** | no_new_privs 是"插件永远不能再提权"的开关 | https://man7.org/linux/man-pages/man2/execve.2.html | man-pages 6.19 |
| `PR_SET_NO_NEW_PRIVS`：设置后**不可撤销**，fork 继承、execve 保留；`/proc/<pid>/status` 有 `NoNewPrivs` 字段 | 建议对所有插件无条件置位；置位不影响"父进程给 ambient 能力"吗？会——ambient 也算 exec 增权，见上条 | https://man7.org/linux/man-pages/man2/PR_SET_NO_NEW_PRIVS.2const.html | man-pages 6.19（Linux 3.5 起） |
| `kill(2)` 权限：持 CAP_KILL，或 sender 的 real/effective uid == target 的 real/saved uid | root 父进程（CAP_KILL）永远能 kill 已降权的子进程 | https://man7.org/linux/man-pages/man2/kill.2.html | man-pages 6.19 |
| `wait(2)` 系列等的是**调用者的子进程**的状态变化，与 uid 无关 | 降权不影响回收/zombie 处理 | https://man7.org/linux/man-pages/man2/wait.2.html | man-pages 6.19 |
| ptrace 访问检查：需要"调用者能力是目标 permitted 的超集且同 user ns"，或目标 ns 里的 CAP_SYS_PTRACE；Yama `ptrace_scope` 默认 **1**（只允许祖先 attach），2=仅 CAP_SYS_PTRACE，3=禁止；**"creating a new user namespace effectively removes the protection offered by Yama"** | 同 uid 的兄弟插件默认不能互相 ptrace，但能自己 unshare user ns 绕过（若允许非特权 userns） | https://man7.org/linux/man-pages/man2/ptrace.2.html | man-pages 6.19 |
| `PR_SET_DUMPABLE`：euid/egid 或 fsuid/fsgid 变化会把 dumpable 重置为 `/proc/sys/fs/suid_dumpable`（默认 0）；不可 dump 的进程不能被 PTRACE_ATTACH，且 `/proc/<pid>` 内文件属主随之变化 | 【推断】插件降权后其 /proc 目录归 root，别的非 root 用户读不到；root 的 svcd 仍可读 | https://man7.org/linux/man-pages/man2/PR_SET_DUMPABLE.2const.html | man-pages 6.19 |
| `proc(5)` hidepid=0/1/2（1=只看自己的 /proc/pid 内文件，2=连目录都不可见），`gid=` 指定豁免组，`subset=pid` | 若坚持"共享一个账号"，hidepid=2 + gid 豁免是唯一能挡住"看别的插件进程"的系统级开关 | https://man7.org/linux/man-pages/man5/proc.5.html | man-pages 6.19 |
| `login.defs`：`SYS_UID_MIN` 默认 **101**，`SYS_UID_MAX` 默认 `UID_MIN-1`（典型 999） | 系统账号的取号窗口就这一段；与发行版已有账号会冲突 | https://man7.org/linux/man-pages/man5/login.defs.5.html | man-pages 6.19 |
| `useradd -r`：系统账号无 /etc/shadow aging，uid/gid 取自 SYS_UID 段，**默认不建 home**（要 -m） | 幂等性差（同名/同 uid 要自己判），失败模式是"号被占" | https://man7.org/linux/man-pages/man8/useradd.8.html | shadow-utils |
| systemd `User=`/`Group=`：系统服务默认 root；非 DynamicUser 时用户必须**服务启动前已存在**（建议 sysusers.d） | 每插件 uid 必须有"谁在什么时候创建"的答案 | https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html | systemd latest |
| systemd `DynamicUser=`：UID/GID 从 **61184…65519** 分配，**不写入 /etc/passwd**，运行时由 nss-systemd 提供；UID 会**回收复用**，因此隐含 RemoveIPC、NoNewPrivileges、RestrictSUIDSGID、ProtectSystem=strict，并要求用 StateDirectory/LogsDirectory/RuntimeDirectory 隔离数据 | 这是"每插件身份"的成熟参考实现：不落盘 + 目录托管 + 明确承认复用风险 | 同上 | systemd latest |
| `CapabilityBoundingSet=` 同时影响 effective/permitted/inheritable；`AmbientCapabilities=` 专给"非特权用户 + 少量能力"的场景，并**自动加 keep-caps securebit** | 这是"非 root uid 插件只拿 CAP_NET_BIND_SERVICE/CAP_NET_ADMIN"的官方配方 | 同上 | systemd latest（AmbientCapabilities 自 229） |
| `unix(7)`：`SO_PEERCRED` 返回**connect/listen/socketpair 时**生效的凭据（`struct ucred`：pid/uid/gid）；`SO_PASSCRED` 让每个收到的消息带 `SCM_CREDENTIALS` | 每插件 socket 用路径名 + 目录 0700 + `SO_PEERCRED` 校验即可；abstract socket 没有文件权限，必须走 SCM_CREDENTIALS | https://man7.org/linux/man-pages/man7/unix.7.html | man-pages 6.19 |

### B. Windows

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `CreateProcessAsUser`：调用者**必须**有 `SE_INCREASE_QUOTA_NAME`，token 非 assignable 时还需 `SE_ASSIGNPRIMARYTOKEN_NAME`；失败 `ERROR_PRIVILEGE_NOT_HELD(1314)` 时改用 `CreateProcessWithLogonW` | LocalSystem 服务默认具备这些特权，路径通 | https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw | MS Learn（本轮抓取） |
| 同上：`hToken` 可以是 `LogonUser` 得到的主令牌，或 `DuplicateTokenEx` 把 impersonation 转 primary | 每插件身份 = 一次登录 + DuplicateTokenEx + CreateProcessAsUser | 同上 | MS Learn |
| 同上文档还写明：`If hToken is a restricted version of the caller's primary token, the SE_ASSIGNPRIMARYTOKEN_NAME privilege is not required` | 用受限令牌可以少一项特权，也是"限制型插件身份"的低成本选项 | 同上 | MS Learn |
| `CreateProcessWithTokenW`：调用者**必须有** `SE_IMPERSONATE_NAME`；失败 1314 时改用 CreateProcessAsUser / CreateProcessWithLogonW | 与服务进程（LocalSystem）配合时同样可行 | https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createprocesswithtokenw | MS Learn |
| `LogonUser`：除 Passport 账号外**不需要** `SE_TCB_NAME`；`LOGON32_LOGON_NETWORK` 返回 impersonation token（需 DuplicateTokenEx 转 primary）；账号需有对应 logon right | 每插件本地用户不需要"Act as part of the OS"，但需要一个能登录的账号（即密码/凭据托管） | https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-logonuserw | MS Learn |
| `CreateRestrictedToken` 三种限制：给 SID 加 deny-only、删特权、加 restricting SIDs（**双重访问检查**：普通 SID 与 restricting SID 都通过才放行） | "per-plugin restricted token"是比重装账号更轻的边界，但限制的是访问不是身份 | https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-createrestrictedtoken | MS Learn（2021-10-13 更新） |
| 同上文档的警告：用受限令牌跑的应用应放在**非默认桌面**，否则可用 `SendMessage`/`PostMessage` 攻击未受限应用 | 插件没有 UI 时这条代价为零；有 UI（反代在浏览器里）时也天然满足 | 同上 | MS Learn |
| job object：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 时，关掉最后一个 job 句柄会终止全部关联进程再销毁 job；嵌套 job 自 Win8/Server 2012 起支持 | 杀插件进程树的正确工具；svcd 崩溃时也能连带清理 | https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects | MS Learn |
| AppContainer 隔离：凭据（每个 user/app 组合独立标识，不能冒用用户）、设备（默认阻断，按需授予）、文件/注册表（默认受限，可给特定持久路径读写）、网络（Internet/Intranet/作服务器，按 capability 授予）、进程（内核对象隔离）、窗口 | 这是"第三方代码"最贴合的边界；但每个维度都要显式授权 | https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation | MS Learn（2025-07-08 更新） |
| 命名管道客户端身份：`GetNamedPipeClientProcessId`（Vista+）取 client PID；文档注记：**AppContainer 内的命名管道只支持同 app 的 UWP 进程之间，且名字必须用 `\\.\pipe\LOCAL\` 语法** | 若插件走 AppContainer，svcd 的管道命名与"跨 app 通信"设计要重做【这是本轮最重要的一条 Windows 约束】 | https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeclientprocessid | MS Learn（2024-02-22 更新） |
| `NetUserAdd` 失败码含 `ERROR_ACCESS_DENIED`（调用者无权）、`NERR_UserExists`、`NERR_PasswordTooShort`；`USER_INFO_1` 需要密码字段 | 建账号需要管理员；本地账号必须有密码 ⇒ svcd 要托管凭据或每次 `LogonUser` | https://learn.microsoft.com/en-us/windows/win32/api/lmaccess/nf-lmaccess-netuseradd | MS Learn |

### C. macOS

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `launchd.plist(5)`：`UserName`/`GroupName` `only applicable for services that are loaded into the privileged system domain`；设了 UserName 不设 GroupName 时用该用户的主组 | LaunchDaemon 可以整体换身份，但**粒度是 job 级**，没有一个"父进程 + 每子进程不同身份"的 launchd 设施 | https://keith.github.io/xcode-man-pages/launchd.plist.5.html | Apple 手册页镜像（Xcode SDK 生成） |
| Background process 四类：Login item（用户域）、XPC service（用户域）、**Launch Daemon（system 域、不能显示 UI）**、Launch Agent（用户域） | LaunchDaemon 是 svcd 的正确形态，也是"默认 root"的来源 | https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/DesigningDaemons.html | Apple 文档归档 |
| `sandbox-exec(1)`：NAME 直接写 `(DEPRECATED)`，"The sandbox-exec command is DEPRECATED. Developers who wish to sandbox an app should instead adopt the App Sandbox feature" | **不要**把插件隔离建在 sandbox-exec 上 | https://keith.github.io/xcode-man-pages/sandbox-exec.1.html | 手册页日期 2017-03-09 |
| XNU `bsd/sys/un.h`：`SOL_LOCAL 0`；`LOCAL_PEERCRED 0x001`、`LOCAL_PEERPID 0x002`、`LOCAL_PEEREPID 0x003`、`LOCAL_PEERUUID 0x004`、`LOCAL_PEEREUUID 0x005`、`LOCAL_PEERTOKEN 0x006`（audit token） | macOS 能判"连上来的是哪个 uid/进程/审计令牌"，等价于 SO_PEERCRED | https://cdn.jsdelivr.net/gh/apple-oss-distributions/xnu@main/bsd/sys/un.h | XNU main @ 2026-11 |
| `posix_spawn(2)` 的 macOS 手册页里没有"设置任意 uid"的属性（无 POSIX_SPAWN_SETUID/SETGID；这是 BSD 系扩展，Linux glibc 也没有） | 【推断】macOS 要给子进程换 uid，只能 fork 后在子进程里 `setgid/setuid` 再 exec；或先降权再 posix_spawn | https://keith.github.io/xcode-man-pages/posix_spawn.2.html | Apple 手册页镜像 |
| `setuid(2)`（BSD 语义）：特权进程可改 real/effective/saved；非特权只能在三者之间切 | 与 Linux 同形，父进程保留 root 即可 | https://keith.github.io/xcode-man-pages/setuid.2.html | Apple 手册页镜像 |

### D. 先例（只列本轮取到一手文档的）

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| systemd 服务单元用 `User=`/`DynamicUser=`/`CapabilityBoundingSet=`/`AmbientCapabilities=`/`NoNewPrivileges=` 组合解决"root 守护进程跑第三方代码" | 先例的答案是**把身份交给服务管理器**，并由管理器托管可写目录（StateDirectory 等） | https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html | systemd latest |
| launchd 用 `UserName`/`GroupName` 在系统域换身份 | 先例的答案是**一个 job 一个身份**，与"一个 svcd 跑多个插件"结构不同 | https://keith.github.io/xcode-man-pages/launchd.plist.5.html | Apple 手册页镜像 |
| Windows AppContainer 用低权令牌 + capability + ACL 表达边界 | 先例的答案是**能力制 + 显式 ACL**，不是"换用户" | https://learn.microsoft.com/en-us/windows/win32/secauthz/appcontainer-isolation | MS Learn |

## 对技术选型的影响

### 1. 四种模型的可落地性（按平台）

| 模型 | Linux | Windows | macOS |
| --- | --- | --- | --- |
| **全 root（现状）** | 零成本；无隔离 | 零成本（LocalSystem）；无隔离 | 零成本；无隔离 |
| **共享一个非特权账号**（svcd 仍是 root，插件统一降到一个 uid） | 最小实现：`setgroups/setgid/setuid` + exec；**缺**：插件间隔离（同 uid 可 kill、可读 environ） | 最小实现：建一个本地用户 + `LogonUser`/`CreateProcessAsUser`；**缺**：同上 + 账号密码托管 | 最小实现：fork+setuid；**缺**：同上；且 LaunchDaemon 换身份要用 job 级 UserName 或自己降权 |
| **每插件独立身份** | 可落地：安装期 `useradd -r`（或 sysusers.d 风格）+ 每插件数据目录 0700；**缺口**：uid 生命周期/回收、无 nss 钩子就没有 DynamicUser 那样的"不落盘" | 可落地两条路：(a) 每插件本地用户 + 密码托管；(b) **AppContainer SID + ACL**（更彻底）；**缺口**：管道命名限制、每个数据目录/日志目录都要 ACL | 可落地但最弱：每插件 uid（dscl/sysadminctl 建号或共享号）；**缺口**：无沙箱可依赖、无 capabilities、sandbox-exec 废弃 |
| **两级（system=root svcd，user=共享或独立）** | 推荐形状：svcd root，插件 uid 由安装期决定；能力按需 ambient/file caps | 推荐形状：svcd LocalSystem，插件 AppContainer 或本地用户 | 推荐形状：svcd root，插件 fork+setuid 到专用 uid |

**结论**：Linux 与 Windows 都能做到"每插件独立身份"，macOS 只能做到"每插件独立 uid + 文件权限"，
**三平台一致的只有"两级模型"**。若产品承诺"每插件隔离"，macOS 上要接受"隔离=uid+文件权限"的语义降级。
【推断，基于上表的一手事实】

### 2. Linux 的三条具体路线与代价

- **A. 只降权，不换号**（全部插件 → 一个 `svcd-plugin` 用户）：一次 `setgroups/setgid/setuid`，
  成本最低；插件之间**不隔离**（可互发信号、读 environ），只隔离了"系统"。
  【文档支撑：kill(2) 的 uid 规则】
- **B. 每插件一个系统 uid**：安装期建号（`useradd -r` 或直接写 passwd），`SYS_UID_MIN..SYS_UID_MAX`（101–999）；
  卸载时删号+删目录；**必须处理 uid 复用**（systemd 文档明确警告 DynamicUser 复用会导致旧文件被新插件读到）。
  【文档：login.defs(5)、useradd(8)、systemd.exec DynamicUser 段】
- **C. 非 root uid + 少量 capabilities**：两条实现路径——
  (i) 安装期 `setcap` 到插件二进制（`security.capability` xattr，需 CAP_SETFCAP）；
  (ii) 父进程 exec 前 `PR_CAP_AMBIENT_RAISE`（要同时在 permitted+inheritable，通常配合 `SECBIT_KEEP_CAPS`）。
  **两条的坑**：脚本插件无效（凭据取自解释器）；exec 到带 file caps/setuid 的程序会清 ambient；
  插件再 spawn 子进程时能力会被继承（ambient 保留），等于**把能力扩散到整棵子树**。【证据：capabilities(7)、commoncap.c、fs/exec.c】

### 3. "同 uid 到底买到什么"（跨平台语义对齐）

- **Linux**：同 uid ⇒ 可 `kill`（kill(2)）、可读 `/proc/<pid>/environ`、`cmdline`、`maps`（READ 模式，Yama 不管）；
  `/proc/<pid>/mem` 与 ptrace 受 Yama 默认 `ptrace_scope=1` 限制（只允许祖先），但**user namespace 可绕过**；
  不可 dump 的进程（降权后的插件）其 /proc 目录属主会变 root，反而挡住同 uid 的其他插件【推断】。
- **Windows**：同用户令牌 ⇒ 默认可 `OpenProcess`（取决于进程 DACL，通常同一用户可读部分信息）、
  可 `TerminateProcess`；真正的边界来自 **restricted token / AppContainer / job object**，不是"同用户"本身。
- **macOS**：同 uid ⇒ 可发信号、可 ptrace（若有权限）；能读的 /proc 等价物是 `proc_pidinfo`/sysctl，
  没有 Linux 那样的 hidepid 挂载开关。**能判身份（LOCAL_PEERCRED），但没有 cheap 的"同 uid 互盲"机制**。
- **一句话**：**"每插件身份"买到的是"插件 A 不能改插件 B 的文件、不能 kill B"，买不到"看不到 B"；
  真正的隐藏要靠 hidepid(namespace)/AppContainer/sandbox，而这三者在三平台上并不对称。**

### 4. 对 svcd 自身操作的影响（必须提前定的契约）

1. **写别人的目录**：插件以 uid X 运行，其数据目录属主是 X；svcd（root）升级/回滚时要写进去，
   必须显式 `chown`/`chmod` 或走"svcd 持有目录、插件只有子目录"的设计。
   systemd 的做法是 `StateDirectory=`（管理器建目录、给属主、退出时清理）——可直接抄这个形状。
2. **日志**：若日志文件由插件自己 open，"root 读日志"没问题，"插件读别人的日志"要防；
   更稳的是 svcd 打开 fd 后传给插件（stdio 已经是这个模型）。
3. **配置属主**：插件配置应由 svcd 写、插件只读（0644 root:root），否则插件可自改配置提权到"下一次以更高身份启动"。
4. **卸载清理**：删 uid 之前先确认没有残留进程（job object / cgroup / `kill` 都行），
   再删数据目录，最后删账号；否则会出现"号删了、文件还在、下次装新插件又拿到同一个号"。
5. **升级保留身份**：插件包升级不应改变 uid（否则数据目录属主要迁移）；这要求 uid 与插件名绑定并持久化。

## 未确认项

1. **Windows 服务 SID（`NT SERVICE\svcd`）的规范文档**。本轮 `SERVICE_SID_TYPE` / Service SID 的
   Microsoft Learn 页面多次 404 / 超时；只知道 SCM 会为服务令牌放入服务 SID，
   `SERVICE_SID_TYPE` 有 unrestricted / restricted / write-restricted 三种（**未取到规范页，未确认**）。
   同理，"SCM 默认以 LocalSystem 启动服务"这一点也没有在本轮取到 `CreateService` 的原文。
2. **AppContainer 能否跑任意原生 exe / 脚本**。isolation 文档只描述隔离维度；
   "AppContainer for legacy apps" 页面本轮 404。**推断**：可以跑，但必须把 exe 与数据目录
   ACL 给 AppContainer SID，且管道命名受限（W8 已证）；**未实测**。
3. **Windows 11 对 AppContainer 的支持差异**。只确认文档在 2025-07 仍更新；没有逐版本核对。
4. **macOS 有没有 capabilities 等价物**。结论是"没有"（只有 uid 0 + entitlement），
   但本轮没有取到一句官方原文，属**推断**。
5. **macOS utun / 路由是否必须 root**。常识是 `ifconfig`/`route` 需 root；NetworkExtension 需专用
   entitlement（第三方守护进程走该框架受限）——两条都**未取到一手文档**，未确认。
6. **hardened runtime / 公证对 spawn 未公证插件二进制的影响**。父进程的 hardened runtime 不约束
   子进程（**推断**）；Gatekeeper 的检查发生在 LaunchServices/`open` 路径，`execve` 直接跑一个
   quarantine 标记的二进制是否弹窗，**未实测**。Apple silicon 上"所有可执行代码必须签名"这条
   （至少 ad-hoc 签名）**未取到规范原文**，但若是真，则插件二进制必须被签名才能在 arm64 上跑。
7. **macOS 每插件用户的建号方式**：`dscl` vs `sysadminctl` 的差异、uid 段（<500 为系统用户）
   与清理方式，本轮未取到官方文档。
8. **musl 静态二进制的影响**：内核层 setuid/file caps/ambient 与动态链接无关（**推断**）；
   静态 glibc 的 NSS（getpwnam）会失效，musl 自带 /etc/passwd 解析——**未取到一手文档**。
   `setcap` 对静态 ELF 有效这一点也**未实测**。
9. **每插件 UDS socket 的默认属主与 mode**：Linux `bind()` 创建的 socket inode 权限受 umask 影响，
   但本轮没找到一句规范原文，**未确认**（设计上应显式 `chmod`/`chown` 而不是依赖 umask）。
10. **OpenWrt / BusyBox 的 `adduser -S` 行为**、**procd 的 ujail 能力**：本轮未取到文档，未确认。
11. **Docker / snapd / PackageKit / polkit / Tailscale / VS Code 扩展的具体做法**：本轮抓取
    Docker 与 snapcraft 文档均只拿到 JS 壳，**未取到一手描述**；这些先例在本笔记中**不作为证据**。
12. **父进程是 root、子进程 uid 变化后 `wait4` 的 `ruid` 统计**、以及 **cgroup v2 下"按 uid 记账"**
    的细节，未核实。
