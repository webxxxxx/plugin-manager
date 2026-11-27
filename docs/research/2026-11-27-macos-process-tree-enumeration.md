# macOS：进程树枚举的三条路（ps / libproc / sysctl）与 pid 复用保护

日期：2026-11-27。针对 #333（macOS 上停止仍只杀进程组，`setsid` 子孙残留）。

问题：`platform::host::process::descendants(pid)` 在 Linux 上读 `/proc` 建父子表、快照 `(pid, start)`；
macOS 分支目前返回空。要在 macOS 上补同一条能力，需要知道：哪些接口能在**一次停止窗口内**拿到
「pid、ppid、进程状态、启动时间」，哪条路的依赖与 ABI 风险最小，以及 pid 复用保护该拿什么值。

## 结论（先给答案）

1. **首选 `libproc`：`proc_listallpids` 拿全表 pid，再对每个 pid 调一次 `proc_pidinfo(PROC_PIDTBSDINFO)`。**
   返回的 `struct proc_bsdinfo` 里 `pbi_pid` / `pbi_ppid` / `pbi_status` / `pbi_start_tvsec` /
   `pbi_start_tvusec` 一次给全：父子关系、僵尸状态、微秒级启动时间。`libc` 0.2.x 在 Apple 上已经绑定
   这些函数与结构体（含 `PROC_PIDTBSDINFO`、`SZOMB`），**不需要新 crate、不需要手写 ABI 布局**。
2. **`ps` 文本路线的代价比代码注释里说的更高**：`ps(1)` 明写 `lstart` 用 `%c`（`strftime(3)`），
   也就是**随 locale 变的人读时间串**。要稳定解析必须钉 `LC_ALL=C`，并自己写一套文本解析器——正是
   `process.rs` 注释里反对「第二套解析器」的那件事，而且比结构体字段更脆。
3. **`sysctl(KERN_PROC_ALL)` 不划算**：`libc` 在 Apple 侧**没有** `kinfo_proc` 绑定（只有
   `CTL_KERN` / `KERN_PROC` / `KERN_PROC_ALL` 常量），`sysctl(3)` 手册页也不描述 `KERN_PROC`。
   走这条路要手写结构体布局，比 libproc 的结构体绑定风险更高，收益相同。
4. **pid 复用保护用 `(pbi_start_tvsec, pbi_start_tvusec)`**，Linux 用 `/proc/<pid>/stat` 的 start ticks；
   两者单位不同、不能跨平台比较，但同一台机器、同一次开机内自洽即可——`Descendant = (u32, u64)`
   的既有形状够用，编码成 `tvsec * 1_000_000 + tvusec`。
5. **僵尸要跳过**：`pbi_status` 是 `p_stat` 值（`SZOMB` = 5），与 Linux 侧跳过 `Z`/`X` 同义。
   父进程未 `wait` 的僵尸仍会出现在全表里，把它算「还活着」会让停止卡到 `KillTimeout`（#111、#334）。
6. **`libproc.h` 自称私有接口**：「This header file contains private interfaces to obtain process
   information. These interfaces are subject to change in future releases.」这是这条路的真实代价：
   符号从 10.5/10.7 起一直存在、`libc` 也按 SDK 头文件绑定，但它不是 Apple 承诺的稳定 API。
   `sysctl` 的 `kern.proc` 同样是未在手册里描述的 MIB。**三条路里没有一条是「公开稳定 API」**，
   差别只在「文本 vs 结构体」和「绑定由谁维护」。

## 1. 枚举全表：`proc_listallpids`

| 断言 | 对 svcd 的含义 | 来源 | 版本/日期 |
| --- | --- | --- | --- |
| `int proc_listpids(uint32_t type, uint32_t typeinfo, void *buffer, int buffersize)`，`int proc_listallpids(void *buffer, int buffersize)`，`int proc_pidinfo(int pid, int flavor, uint64_t arg, void *buffer, int buffersize)` | 三个函数都是 `libproc` 导出；`libc` 已绑定（`proc_listallpids` / `proc_listpids` / `proc_pidinfo`） | https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.h | xnu main @ 2026-11 |
| `proc_listpids` 返回**字节数**，内部失败时返回 0；`proc_listallpids` 返回**pid 个数**（`bytes / sizeof(int)`）；`proc_listchildpids(ppid, …)` 是 `PROC_PPID_ONLY` 的便捷包装 | 尺寸探测用 `proc_listpids(PROC_ALL_PIDS, 0, NULL, 0)`；要「全部 pid」可直接用 `proc_listallpids`，成功时是个数、不是字节 | https://github.com/apple-oss-distributions/xnu/blob/main/libsyscall/wrappers/libproc/libproc.c | xnu main @ 2026-11 |
| `PROC_ALL_PIDS` = 1、`PROC_PIDTBSDINFO` = 3 | 常量在 `sys/proc_info.h`；`libc` 只绑了 `PROC_PIDTBSDINFO`，`PROC_ALL_PIDS` 要自己写 `1` 或直接用 `proc_listallpids` | https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h | xnu main @ 2026-11 |
| 头文件未说明 `buffer = NULL` 时的返回值是否就是「所需字节数」；`proc_listpids` 在失败时返回 0 而非 -1 | 尺寸探测要按「返回 0 → 当作拿不到，降级为空快照」处理，不能当错误码区分 | 同上（`libproc.c`） | xnu main @ 2026-11 |

## 2. 每个 pid 的信息：`proc_pidinfo(PROC_PIDTBSDINFO)`

| 断言 | 对 svcd 的含义 | 来源 | 版本/日期 |
| --- | --- | --- | --- |
| `struct proc_bsdinfo` 字段：`pbi_flags`、`pbi_status`、`pbi_xstatus`、`pbi_pid`、`pbi_ppid`、`pbi_uid`/`gid`、…、`pbi_pgid`、…、`pbi_start_tvsec`、`pbi_start_tvusec` | 一次调用拿到父子关系、状态与启动时间；`pbi_pgid` 还能顺带确认「是否已经不在组里」 | https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/proc_info.h | xnu main @ 2026-11 |
| `libc` 的 `proc_bsdinfo` 绑定与头文件逐字段一致（含 `pbi_start_tvsec/usec: u64`） | 不需要手写结构体；升级 SDK 时字段漂移由 `libc` 承担 | `libc` 0.2.191 `src/unix/bsd/apple/mod.rs`（本机 `Cargo.lock` 锁定的版本） | libc 0.2.191 |
| `proc_pidinfo` 返回**返回的字节数**，内部失败返回 0；Apple 自己的 `proc_name` 只判 `retval != 0` | 严格判据应是 `retval >= size_of::<proc_bsdinfo>()`；进程在枚举与查询之间退出时返回 0，**跳过即可**（与 Linux 侧「stat 读不到就 continue」同构） | `libproc.c`（`proc_name` 一处用法） | xnu main @ 2026-11 |

## 3. 僵尸与 pid 复用

| 断言 | 对 svcd 的含义 | 来源 | 版本/日期 |
| --- | --- | --- | --- |
| `pbi_status` 是 `p_stat` 值；同族 `proc_bsdshortinfo` 的 `pbsi_status` 注释明写「`p_stat` value, `SZOMB`, `SRUN`, etc」 | 判僵尸用 `pbi_status == SZOMB`（Apple 的 `SZOMB` = 5）；与 Linux 侧跳过 `Z`/`X` 对齐 | `sys/proc_info.h`（`proc_bsdshortinfo` 注释）、`libc` 0.2.191 `SZOMB: u32 = 5` | xnu main / libc 0.2.191 |
| `pbi_start_tvsec` / `pbi_start_tvusec` 是进程启动时刻（秒 / 微秒） | 编码成 `tvsec * 1_000_000 + tvusec` 存进 `(u32, u64)`；Linux 的 start ticks 与它单位不同，**只在同平台内比较**，不要跨平台对齐数值 | `sys/proc_info.h` 字段名 + 现有 `Descendant` 语义（`process.rs` 注释） | xnu main @ 2026-11 |
| 全表枚举与逐 pid 查询之间进程会变 | 两次调用之间的竞态只表现为「查到 0 字节 / pid 已不存在」，按跳过处理；这是与 Linux `/proc` 扫描同级的已知窗口（ADR-0031 已接受） | `libproc.c`；ADR-0031 | 2026-11 |

## 4. `ps` 路线：`lstart` 是 locale 相关的人读字符串

| 断言 | 对 svcd 的含义 | 来源 | 版本/日期 |
| --- | --- | --- | --- |
| `lstart`：「The exact time the command started, using the `%c` format described in `strftime(3)`」 | `%c` 是「当前 locale 的日期时间表示」；中文/其他 locale 下格式与字段顺序都会变，必须钉 `LC_ALL=C` 才可解析 | https://keith.github.io/xcode-man-pages/ps.1.html（Apple 手册页镜像，本仓既有笔记同源） | Darwin ps(1) |
| `ps -A -o pid=,ppid=` 的数值列本身与 locale 无关 | 数值列可解析；但时间列不可，除非强制 C locale | 同上 | Darwin ps(1) |
| `ps` 是子进程：要绝对路径、要 fork+exec、`ps` 自身输出受 `kern.proc` 可见性影响 | 停止路径多一次进程创建；守护进程环境下应写 `/bin/ps`，不要依赖 `PATH` | 推断（基于 ps 是 `/bin` 下的独立可执行文件） | — |
| 已有项目因 `lstart` 的 locale 差异在 macOS 上强制 C locale 才稳定解析 | 说明这是实践里踩过的坑，不是理论风险 | https://github.com/Mesh-LLM/mesh-llm/commit/f20cd10941de364fcdc7ed45eb3ae5dc20e9d37c；同类：https://lists.freebsd.org/pipermail/freebsd-bugs/2020-June/094842.html | 二手来源，2026-11 检索 |

## 5. `sysctl(KERN_PROC_ALL)` 路线

| 断言 | 对 svcd 的含义 | 来源 | 版本/日期 |
| --- | --- | --- | --- |
| `libc` 在 Apple 侧只有 `CTL_KERN`(1) / `KERN_PROC`(14) / `KERN_PROC_ALL`(0) 等常量，**没有** `kinfo_proc` 结构体绑定 | 走这条路要手写 `kinfo_proc`/`extern_proc` 布局；任何字段漂移都只能在真机上炸，不在编译期 | `libc` 0.2.191 `src/unix/bsd/apple/mod.rs` | libc 0.2.191 |
| `sysctl(3)` 手册页没有描述 `KERN_PROC` / `kinfo_proc` | 「kern.proc」是未在手册里承诺的 MIB，语义只能读 XNU 源码 | https://keith.github.io/xcode-man-pages/sysctl.3.html（抓取后全文无 `KERN_PROC`） | Darwin sysctl(3) |

## 6. 权限

| 断言 | 对 svcd 的含义 | 来源 | 版本/日期 |
| --- | --- | --- | --- |
| `proc_pidinfo` 对同 uid 进程可用；跨 uid 的字段可见性按「私有接口」处理，未逐项核实 | svcd 在 macOS 是 root LaunchDaemon，主用例不受影响；**user mode（非 root）下能拿到多少未核实**，实现要按「查不到就跳过」写 | `libproc.h` 的私有接口声明 | 未核实 |
| `libc` 的 Apple 绑定随 `target_os = "macos"` 生效，`cargo build --target aarch64-apple-darwin` 可编译 | 编译面由 CI 的 macOS job 兜（只 build + usage 冒烟，不跑测试） | `.github/workflows/build.yml` | 2026-11 |

## 对 #333 的含义（一句话）

走 libproc：**树遍历算法抽成平台中立函数（假行数据可在 Linux/Windows 上测），macOS 侧只留
「枚举 → 行」这一小段**；`start_time` 同票补上（否则快照是空转），僵尸按 `SZOMB` 跳过，
pid 复用用 `(tvsec, tvusec)`。

## 未决问题

1. `libproc` 明确写着「private interfaces, subject to change」，`ps`/`sysctl` 也不在公开契约里。
   选 libproc = 接受「SDK 里有头文件、符号长期存在、但 Apple 不承诺稳定」；选 `ps` = 接受
   「公开命令 + 文本解析 + 强制 C locale」。这需要维护者拍板（#333 Q1）。
2. 非 root（user mode）下 `proc_pidinfo` 对跨 uid 进程能拿到什么，本文未核实；需要在 Mac 上
   以普通用户跑一次才能确认降级行为。
3. `pbi_status` 与 `p_stat` 的同一性来自同族结构体注释，未在 `proc_bsdinfo` 上直接写明；
   真机验证时顺手确认一次 `SZOMB` 判定即可。
