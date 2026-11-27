# 停止的树不只是进程组：`setsid` 出去的子孙也要一起带走

删掉一个**正在跑**的 sshd 插件之后，`ps` 里仍有 `dropbear` 与 `sftp-server`。原因是停止只对进程组发信号（`platform::host::process::terminate_tree` = `kill(-pid, SIGTERM)`；Unix 上的 `Tree` 是空实现，注释里写着「组就是树」），而 dropbear 每接受一个连接就 fork 一个子进程，这个子进程无条件 `setsid()`
（ssh 插件仓库的 `vendor/dropbear-2022.83/svr-main.c:334`，pty 会话在 `sshpty.c:284` 再 `setsid` 一次）。它因此离开了插件的进程组：leader（监听进程）被 SIGTERM 打死，连接子进程与它拉起的 shell / `sftp-server` 继续跑，还持有已经从磁盘删掉的插件文件。

这不是 sshd 特有：任何会 `setsid`（或双 fork 后脱离会话）的插件都一样。supervisor 已有的「树里任一进程仍存活时服务保持 `Running`」（当时的 `exit_while_group_alive`，现在叫 `exit_while_tree_alive`）也只看得见组。

## Decision

- **停止的树 = 进程组 ∪ 停止那一刻快照到的子孙**。树里任一进程仍存活时服务保持 `Running` / `Stopping`，不报 `Stopped`。
- **快照时机是任何信号之前**。`Slot::request_stop` 在 `Exited` / SIGTERM 之前按 `/proc/<pid>/stat` 的父链递归收集子孙，记下 `(pid, start time)`。leader 一旦退出，它的孩子会被 init 接管，父子关系就再也读不出来了 —— 所以快照只能在停止开始时做。
- **信号分两次，和已有的两段超时对齐**：polite 阶段对组与快照发 SIGTERM；`KillTimeout` 之后对组与快照发 SIGKILL。快照里的进程如果已经换成了别的进程（start time 对不上），跳过不发信号。
- **快照只属于这一次停止**：进程清空时（`clear_process`）丢弃，下一次停止重新快照。
- **枚举父链的宿主各自落地**：Linux 走 `/proc`；macOS 走 libproc（`proc_listpids` + `proc_pidinfo(PROC_PIDTBSDINFO)`，字段 `pbi_ppid` / `pbi_status` / `pbi_start_tvsec|tvusec`，#333）；Windows 的 job object 本来就是整棵树，`descendants` 返回空、行为不变。macOS 不走 `ps`：`lstart` 是 `strftime(3)` 的 `%c`，随 locale 变；也不走 `sysctl KERN_PROC`：`libc` 没有 `kinfo_proc` 的 Apple 绑定，要手写结构体布局。代价是 libproc 自称「private interfaces … subject to change」。

## Considered Options

- **把 daemon 变成 child subreaper + 收尸**：能覆盖「leader 崩溃后遗留的子孙」，但要接管所有孤儿的回收语义、影响 daemon 退出，超出「停止要干净」的范围。
- **cgroup（Linux）**：天然是树，但要求 cgroup v2 可写、与 OpenWrt 的挂载方式冲突，且 Windows/macOS 没有同构物。
- **在插件脚本里自己杀**：契约明确「终止权归宿主」，让第三方脚本负责只能得到不一致行为（dropbear 的 `stop` 就不该发信号）。
- **只按进程组杀**（现状）：就是这次的缺陷。

## Consequences

- `svc stop` / 卸载之后不会再留下插件的连接子进程；代价是停止时多一次进程表扫描（Linux 读几百个 `stat` 文件，macOS 一次 `proc_listpids` 加每个 pid 一次 `proc_pidinfo`，都只发生在停止路径）。
- 杀子孙用的是同一个 SIGTERM→SIGKILL 阶梯：会话里的 shell 有机会在 SIGTERM 里退出，10 秒后仍在的才会被 SIGKILL。
- 崩溃路径不在本 ADR 内：leader 崩溃时遗留的 `setsid` 子孙仍只受「组」约束，重启的是一个新 leader。
- 停止期间的判定变成「树空了才算完」，插件若有一个赖着不走的子孙，服务会停在 `Stopping` 直到 `KillTimeout` 把它打死，日志会记下被带走的子孙数量。
- macOS 上随之有了可比较的启动时间（libproc 的微秒级 `pbi_start_tvsec/usec`），接管（ADR-0005）的 pid 复用判定从 `Unknown`（信任）升到 `Same` / `Reused`：重启后一条陈旧的 runtime 记录不会再被一个复用了 pid 的无关进程接管。代价是 libproc 是私有 SPI，「subject to change」的字段漂移只能在编译或真机上发现。
