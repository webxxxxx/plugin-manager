# oneshot / RemainAfterExit：一次性服务的完成态

core 服务里有一类"干完就退出"的：建隧道（`wg-start.sh`）、挂 NAS（`mount-sshfs.sh`）。它们被监督的进程正常退出 0 之后，状态机把它当成「停了一次」——`state=stopped`，于是 `svc list` 说服务没起、`svc start <依赖它的服务>` 会把一次性脚本再跑一遍、`svc stop` 也只是碰巧把 stop 动作异步拉起来。shell 版 `svcd` 的处理是：`RESTART=never` / `on-failure` 下成功退出 → `state=running`、`pid` 为空（systemd 的 `RemainAfterExit`）。

## Decision

- `Service`/`Machine` 增加 `remain_after_exit`。`restart != always` 且退出码为 0 时，服务保持 `Running`、`pid=None`；非 0 退出仍是失败路径（`never` → `Failed`，其余重试）。
- **core JSON 显式声明**：`"remain_after_exit": true`，默认 `false`（Rust 原生服务不为历史行为买单）。
- **drop-in shell 脚本沿用 shell 约定**：`restart != always` 即视为一次性（`TYPE=oneshot`、`RESTART=never`、缺省 `on-failure` 都命中），与服务版 `svcd` 对齐。
- **插件恒为 `false`**：插件契约是被监督进程常驻；退出 0 说明它不该再报 running，也不存在可调开关。
- **与通道对端互斥**：`remain_after_exit: true` 不能和通道对端同时声明——`exec.ready: notify`（省略 `ready` 也默认 notify）或 protocol 3；定义加载即拒绝。通道跟着进程走，进程可以退出就没人接调用；一次性服务要声明 `exec.ready: exec`（#197）。
- `svc start` 对「完成态」允许重跑（`Running` 且无 pid），与 shell 版一致；对正在运行的进程仍是幂等 no-op。
- `svc stop` 对完成态不发信号、不进入停止/强杀窗口：有 stop 动作就先跑它，然后 `StopSettled` 直接回 `stopped`。
- 运行态记录（ADR 0005）落盘后，daemon 重启会把「完成态」按记录恢复成 `Running`/无 pid，**不重跑**一次性工作；跨 boot 的记录仍被丢弃，开机按 `enabled` 重新执行。
- 依赖关系里，完成态算 `Running`：`start_layer` 不会重复执行一次性依赖。

## Considered Options

- **照 shell 原样让所有 `restart != always` 的干净退出都保持 running（拒绝）**：对 core 脚本是对的，但插件默认也是 `on-failure`，一个协议插件退出 0 会被报成 running 且 RPC 已断，比 stopped 更误导。
- **只认 `TYPE=oneshot`（拒绝）**：`05-nas` 只写了 `RESTART=never`，会被漏掉；shell 版对 `never`/`on-failure` 一律按 done 处理。
- **用 `restart=never` 兼任 oneshot（拒绝）**：`never` 表达的是「失败也不重启」，与「效果留存」是两件事；`wg-start` 失败时需要失败态，成功时需要完成态。
- **完成态继续存一个哨兵进程（拒绝）**：白白占一个进程/一个 pid，还会让「无进程但 running」这个既有模型变形。

## Consequences

- `svc status` / `svc list` 里一次性服务显示 `running`、`PID` 为 `-`；`details.last_exit=0` 可区分「刚跑完」和「进程在跑」。
- 一次性服务必须自己保证 `stop` 动作幂等且真的释放资源——完成态下进程早就不在了，停止只能靠 stop 动作。
- 没有 stop 动作的一次性服务，`svc stop` 只改状态（没有可杀的东西），不会等 10 秒强杀窗口。
- `svc restart` 对完成态 = 重跑一次；对运行中的进程仍是「停 → 起」。
- 完成态在 daemon 崩溃后按记录恢复（`run/<id>.json` 中 `pid` 为空、`state=running`），因此不会因为一次 svcd 重启重复建隧道/重复挂载；机器重启后记录因 boot id 不符被丢弃，按 `enabled` 重跑——与 shell 版行为一致。优雅重启（`svcd shutdown`/服务管理器 stop）会先停服务、删记录，下次启动重跑。
- 插件不受影响：退出 0 仍是 `stopped`，退出非 0 仍按退避重试。

## 补（2026-11-18，`#183`）

启动收敛按「是否通道对端」区分：上文的「完成态」只属于**不欠活通道**的服务（`ready: exec` 的 protocol 1/2）。通知型插件或 `protocol: 3` 的服务在启动窗口内落到 `Stopped` 是**失去通道**，`svc start` 返回失败并点名退出码，而不是报成功。判定在 `check::start::start_step`，通道对端的定义见 `GLOSSARY.md`。
