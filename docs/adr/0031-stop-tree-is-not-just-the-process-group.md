# 停止的树不只是进程组：`setsid` 出去的子孙也要一起带走

删掉一个**正在跑**的 sshd 插件之后，`ps` 里仍有 `dropbear` 与 `sftp-server`。原因是停止只对进程组发信号（`platform::host::process::terminate_tree` = `kill(-pid, SIGTERM)`；Unix 上的 `Tree` 是空实现，注释里写着「组就是树」），而 dropbear 每接受一个连接就 fork 一个子进程，这个子进程无条件 `setsid()`
（`vendor/dropbear-2022.83/svr-main.c:334`，pty 会话在 `sshpty.c:284` 再 `setsid` 一次）。它因此离开了插件的进程组：leader（监听进程）被 SIGTERM 打死，连接子进程与它拉起的 shell / `sftp-server` 继续跑，还持有已经从磁盘删掉的插件文件。

这不是 sshd 特有：任何会 `setsid`（或双 fork 后脱离会话）的插件都一样。supervisor 已有的「组内任一进程仍存活时服务保持 `Running`」（`exit_while_group_alive`）也只看得见组。

## Decision

- **停止的树 = 进程组 ∪ 停止那一刻快照到的子孙**。树里任一进程仍存活时服务保持 `Running` / `Stopping`，不报 `Stopped`。
- **快照时机是任何信号之前**。`Slot::request_stop` 在 `Exited` / SIGTERM 之前按 `/proc/<pid>/stat` 的父链递归收集子孙，记下 `(pid, start time)`。leader 一旦退出，它的孩子会被 init 接管，父子关系就再也读不出来了 —— 所以快照只能在停止开始时做。
- **信号分两次，和已有的两段超时对齐**：polite 阶段对组与快照发 SIGTERM；`KillTimeout` 之后对组与快照发 SIGKILL。快照里的进程如果已经换成了别的进程（start time 对不上），跳过不发信号。
- **快照只属于这一次停止**：进程清空时（`clear_process`）丢弃，下一次停止重新快照。
- **只在能枚举父链的宿主上做**：Linux 走 `/proc`；Windows 的 job object 本来就是整棵树，`descendants` 返回空、行为不变；macOS 没有 `/proc`，`ps` 会引入第二套解析器与它的测试，暂不实现（已知缺口，见 #332 的结论）。

## Considered Options

- **把 daemon 变成 child subreaper + 收尸**：能覆盖「leader 崩溃后遗留的子孙」，但要接管所有孤儿的回收语义、影响 daemon 退出，超出「停止要干净」的范围。
- **cgroup（Linux）**：天然是树，但要求 cgroup v2 可写、与 OpenWrt 的挂载方式冲突，且 Windows/macOS 没有同构物。
- **在插件脚本里自己杀**：契约明确「终止权归宿主」，让第三方脚本负责只能得到不一致行为（dropbear 的 `stop` 就不该发信号）。
- **只按进程组杀**（现状）：就是这次的缺陷。

## Consequences

- `svc stop` / 卸载之后不会再留下插件的连接子进程；代价是停止时多一次 `/proc` 扫描（一次读取几百个 stat 文件，只发生在停止路径）。
- 杀子孙用的是同一个 SIGTERM→SIGKILL 阶梯：会话里的 shell 有机会在 SIGTERM 里退出，10 秒后仍在的才会被 SIGKILL。
- 崩溃路径不在本 ADR 内：leader 崩溃时遗留的 `setsid` 子孙仍只受「组」约束，重启的是一个新 leader。
- 停止期间的判定变成「树空了才算完」，插件若有一个赖着不走的子孙，服务会停在 `Stopping` 直到 `KillTimeout` 把它打死，日志会记下被带走的子孙数量。
