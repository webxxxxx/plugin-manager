# daemon 重启语义：运行态落盘 + 启动校验/接管

svcd 被 kill -9、崩溃或被系统重新拉起时，旧实现的处理是「一起死」：Linux 用 `PR_SET_PDEATHSIG`、Windows 用 `KILL_ON_JOB_CLOSE` 把服务树带走。于是 daemon 的一次异常退出会让所有服务闪断一次；手工启动（`enabled=false`）的服务重启后不再回来；`svc list` 在 daemon 不在时只能显示 stopped，看不出上一秒在跑什么。唯一「干净」的替代是给每个服务再套一个监督进程（shell 版 svcd 的做法），但那是每服务一个常驻进程 + 一套独立的会话/锁，与单进程事件的交叉平台实现冲突。

## Decision

- **运行态落盘**：每个受管服务一个 `<data>/run/<id>.json`，字段 `boot / state / pid / starttime / since / ready_since / failures / last_exit`。写入是 `tmp + rename` 原子替换；只在 `(state, pid, failures)` 变化时写；进入 `stopped`/`failed` 且没有 pid 时删掉记录（没有可接管的对象）。
- **启动校验**：只有同时满足三条才接管——`boot` 与当前 `/proc/sys/kernel/random/boot_id` 相同、`pid` 还活着、`/proc/<pid>/stat` 的 starttime 与记录一致（防 pid 复用）。任一不满足就丢弃记录，按正常开机流程处理。
- **接管语义**：记录是 `running`/`starting` 的活进程 → 直接进 `Running`（不重启、不发信号）；记录是 `stopping` 的残留 → 按 `Running` 接管后走一次**正常停止**（先跑 stop 动作，再 SIGTERM/SIGKILL），不丢服务的清理逻辑；记录是 `running` 且 `pid` 为空（一次性服务已完成，见 ADR 0006）→ 恢复完成态、不重跑；其余状态没有 pid 可接管。
- **孤儿记录清理**：启动时和 `svc reload` 末尾扫描 `<data>/run`，删掉 registry 里已不存在的 `<id>.json`。若记录里的 pid 仍有效（pid + starttime 校验通过），先对进程组发 SIGTERM 再删，覆盖「daemon 停机期间定义被删」的情况。
- **运行期重扫**：`services.d` 的指纹（名字 + mtime + size）每 `SVCD_RESCAN_INTERVAL_MS`（默认 5s，0 关闭）比对一次，变了就复用 `reload` 的 reconcile：新增定义按 `enabled` 启动、删除定义停掉并遗忘。`svc reload` 仍保留，用于手动立即触发。
- **Unix 不再给服务进程挂 parent-death 信号**（`proc::configure_service`），服务在 daemon 死后继续运行，等下一个 daemon 接管；**hooks 仍保留** `PR_SET_PDEATHSIG`，它们不该比 daemon 活得久。Windows 保持不变：Job Object 随 daemon 关闭而终止整棵树，没有可接管的进程（记录只用于清理）。
- **开机等待 + reload**：daemon 启动时若 `services.d` 还不存在，最多等 10 秒（`SVCD_BOOT_SCAN_WAIT_MS`，0 关闭），覆盖只读 rootfs 上 overlay 晚挂载；之后仍可用 `svc reload` 重新扫描并对账（新增/删除/修改定义）。
- **策略与自启解耦**：`enabled` 只决定开机是否启动；崩溃是否重启由服务级 `restart=always|on-failure|never` 决定（core 定义字段，drop-in 脚本用 `RESTART=` / `TYPE=oneshot` 声明，插件默认 `on-failure`）。

## Considered Options

- **保持父死子亡（拒绝）**：实现简单，但 daemon 崩溃 = 全部服务闪断，手工启动的服务永远丢失，且 `svc` 无法解释「上一秒还在跑」。
- **落盘但从不接管（拒绝）**：只能当审计日志用；重启后仍要靠 enabled 重拉，接管不了存活进程，反而多一份可能与现实不一致的状态。
- **每服务一个监督进程（拒绝，shell 版做法）**：接管不需要任何落盘，但每服务多一个常驻进程、独立会话与锁，且 Windows 没有同构物；与单进程事件循环的跨平台实现冲突。
- **用 pidfile 而不校验 starttime/boot（拒绝）**：重启后 pid 会被复用，把无关进程当服务接管（或杀掉）比不接管更糟。

## Consequences

- daemon 崩溃后，服务在 inittab/systemd 重新拉起 daemon 的窗口期内处于「无人监督」状态；窗口通常 <1 秒，但若 daemon 再也起不来，服务会一直跑下去（与 shell 版一致，是接管语义的代价）。
- 防重复实例依赖「记录 + pid + starttime」三件套：记录损坏或 starttime 不可读时宁可丢弃记录（可能重启服务），也不错误接管。
- `ready=notify` 的插件被接管后没有可用的 stdin 管道：状态显示 `Running`，但 `tools/call` 会因通道不可用而失败，直到下一次显式 restart。这是「进程存活但协议已断」的已知限制。
- Windows 行为：活进程仍由 Job Object 随 daemon 终止，重启后按 `enabled` 重拉，不接管；但「完成态」记录（无进程）在所有平台都会恢复成 `running`/无 pid，避免 daemon 重启导致一次性服务重跑。
- 优雅停机（`svcd shutdown` / SCM stop）仍会先停所有服务再退出，记录随之清空，不会留下需要接管的进程。
