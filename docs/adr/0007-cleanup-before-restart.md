# 重启前清理：失败重试与稳定崩溃都先跑 stop 动作

被监督进程退出后，新进程起来之前应该先执行服务自己的 `stop` 动作（撤 iptables 规则、卸载临时挂载、清 pid 文件）。shell 版 `svcd` 的 worker 在每次重进循环前都调用 `run_stop`，包括正常启动失败、崩溃、以及运行很久之后才崩溃的情况。Rust 版原先只在显式 `svc stop` 时跑，重试和「稳定运行 ≥10s 后的崩溃重启」都直接重生——半成品资源会带进下一轮。

## Decision

- **失败重试**：状态机进入 `Backoff` 时（`Action::ArmBackoff`）启动 stop 动作；退避到点后如果钩子还在跑，先记 `backoff_pending`，钩子结束再 `BackoffElapsed` → spawn。没有 stop 动作就按原节奏重启。
- **稳定崩溃**：`Exited + Running + 已跑满 10s` 不再直接 `Spawn`，改为保持 `starting`、产出 `Action::Cleanup`；槽启动 stop 动作，钩子结束后发 `Event::RestartReady` 才 spawn。状态全程是 `starting`，不借用 `backoff`。
- **抢占规则**：清理期间收到 `stop`，以 stop 为准——`Stopping` 分支清掉待重启标记，钩子结束后按停止收敛（无 pid 直接 `stopped`），不会在停止之后又拉起一个新进程。
- **时间边界**：清理耗时由钩子自己的 `timeout` 兜底（默认 10s、上限 300s）；超时后照常重启。稳定崩溃不额外加延迟——稳定运行已证明不是崩溃循环，不需要背退避的语义。
- **进程模型不变**：stop 动作本来就是一个短命子进程，清理发生在「旧进程已退出、新进程未 spawn」的窗口里，不新增常驻进程，也不改变进程组/组杀。

## Considered Options

- **复用 0 延迟退避（`ArmBackoff{0}`）**：代码最少，但 `backoff` 在契约里是「失败后等待重试」，正常重启会短暂显示 `backoff`，监控和页面容易误判；拒绝。
- **同步跑 stop 动作再重生**：语义最直接，但会阻塞单线程事件循环（钩子最长 300s），其他服务全部停摆；拒绝。
- **稳定崩溃不做清理**：等于继续把资源泄漏留给下一轮；拒绝。
- **清理后再等一个固定延迟（shell 的 `SVC_RESTART_DELAY`）**：稳定崩溃没有防抖动需求，固定延迟只是拖慢恢复；不加，需要时由钩子自己控制。

## Consequences

- 有慢 stop 动作的服务，重启延迟 = 钩子耗时（最长 300s）；日志会记录钩子退出码/超时。
- stop 动作必须是幂等的：它会在进程已经退出后运行，也可能在失败重试、稳定崩溃、显式 stop、关机等多个路径被调用。
- 没有 `lifecycle.stop` 的服务行为与之前完全一致（立即重启）。
- 状态机多了一个 `RestartReady` 事件；`starting` 的语义扩展为「启动中或重启清理中」，`svc status` 仍显示 `starting`。
