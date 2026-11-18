# 调用统计记在槽里，落在日志

## Status

Accepted（2026-11-18）。修订 [ADR-0022](0022-log-lines-carry-time-level-and-source.md)：`source` 增加 `stats`，日志文件从三个变四个。

## Context

- 今天没有任何按服务的调用计数。唯一的先例是 raw 传输的 `RawStats`（rawhub.rs:53-104），那是跨线程共享才需要原子。
- 槽里没有“开始时间”：`SlotHandle::submit` 不收 `now`（slot.rs:138-173），超时窗口在 drain 时才起（slot.rs:561-637），`PendingCall` 只有 `deadline`。完成侧 `complete_pending` 也不收 `now`（:1003/:1051），但它的调用方 `drain_frames(now)` 有（:919-928）。
- `slot.handle()` 每次调用新建（daemon.rs:1379），但 `Daemon` 自己持有 `slots`，状态投影 `status_for` 就是直接读 `slot.machine`（daemon.rs:1423-1431）。所以计数器可以是槽上的普通字段，不需要 `Arc` 或原子。
- 内置 `svc` 不走槽：`exec: None`，每次调用新建一个 `BuiltinWire` 同步应答（builtin_adapter.rs:20-104），没有 deadline、没有 pending。
- 预算算术：一行统计约 100 字节，10 个服务、每分钟一行是 1.44 MB/天，而 `svcd.log` 的预算是 64 KiB × 3 = 192 KiB。统计写进运维日志，3 小时就把事件历史冲干净。

## Decision

- **记在槽里**：`calls`、`errors`（按内核失败类别分桶）、`duration_sum`、`duration_max`、`last_at`；`in_flight` 在发线时直接读 `pending.len()`，不另存计数器。按能力的细分只留调用最多的 32 个，其余进 `other` 桶——能力名来自插件 manifest，是外部输入。
- **耗时口径**：从槽把 `tools/call` 帧写给插件那一刻，到完成/超时/失败。**不含**控制通道里的排队时间；那个时间今天不可观测，要测就得在信封里加字段，不值得。
- **发线在监督循环**：`tick(now)` 里按 `stats_interval_secs`（默认 300，0 = 关闭）判到期，每个服务发一行，走 `stats.log` 的 `LogSink`，尺寸与份数沿用 `log_max_bytes` / `log_rotations`（默认 64 KiB × 3），不新增尺寸键。
- **行形状沿用 ADR-0022**：

  ```
  2026-11-18T06:00:00.123Z INFO  stats          demo calls=42 errors=3 in_flight=1 sum_ms=812 max_ms=120 age_s=3600
  ```

  `age_s` 是这批计数的累计秒数：daemon 重启或服务重建后从 0 开始，行里自证清零。
- **读取只加一条**：`svc logs stats`，沿用 `svc logs svcd` 的特殊 id 先例，不新增 CLI 动词、不新增 HTTP 路由、不进 `Status` 字段。
- **不持久化**：纯内存，重启归零。日志行本身就是时间序列。
- **内置 `svc` 不计数**：它没有槽，进程内同步应答，耗时没有意义。

## Considered options

- **记在契约边界**：所有载体统一，但要在内核里传时钟才能算耗时，而且看不到 in-flight；槽本就拥有 pending 表与循环里的 `now`。
- **进 `Status.details.stats` 并暴露 HTTP**：能查“此刻多少”，但多一条真相来源，列表接口的 payload 还会变胖；选择日志后，`svc logs stats` 一条命令就够。
- **写进 `svcd.log`**：最省一个文件，但按上面的算术会吃掉运维事件历史。

## Consequences

- 10 个服务、5 分钟周期下，`stats.log` 的 192 KiB 预算约保留 16 小时；要留更久就调大 `stats_interval_secs` 或 `log_max_bytes`，没有第三个旋钮。
- 逐行解析依赖 ADR-0022 的列契约；发线走同一个 `LogSink`，将来加背压时统计行自动跟着排队，不会另开一条 I/O 路径。
- 失败类别用 S5 的类别；出现未知类别时进 `other`，不能让发线因为一条没见过的类别失败。
- 不在本决定内：跨重启的累计、按能力的百分位、HTTP / 控制台视图、告警阈值。
