# 日志行：时间、级别与来源

## Status

Accepted（2026-11-18）。细化 `docs/architecture.md` §5 的 `platform/log`，并收掉 [ADR-0020](0020-time-is-a-parameter-not-a-trait.md) 明确留在范围外的「log 注入」。实施切片见 spec `platform 可观测性：日志行与级别`。

## Context

- 日志写入口是 `log::svcd(&str)`（后由 `error!/warn!/info!/debug!/trace!` 宏族接替）。116 处调用、19 个模块，消息是自由文本（`"svcd: {}: spawn failed: {}"` 之类），没有时间戳、级别、来源。
- 两类写入者写两个文件：daemon 自己的行写 `svcd.log`，插件与钩子的 stdout/stderr 原样写 `<id>.log` / `<id>-hooks.log`（`src/wire/contract/stdio.rs:227`、`src/wire/contract/reactor.rs:819`）。两边都没有时间戳，一次故障里「插件先挂还是 svcd 先报错」无法判断。
- 插件行在 Unix 有 64 KiB 上限（`reactor.rs:36`）；一行无换行且超限时 `drain_lines` 返回 false → `Stop::Single`（`reactor.rs:589`），**整个 log source 被静默丢弃**，之后该插件的输出全部消失，也没有一行日志说明。Windows 与钩子走 `log_lines` 线程（`stdio.rs:210`），那条路径完全没有上限。
- 写失败一律 `let _ = sink.line(...)`（`reactor.rs:882`、`stdio.rs:230`、`log.rs:177`），静默丢弃；`line()` 每行同步 `flush`（`log.rs:80-82`），写盘发生在监督循环线程上。
- 读取面有三个：`log::tail`（`log.rs:187`）、HTTP 的 `cap_log`（`face/api.rs:480`）、Web 控制台。`tests/cli.rs:342` 与 `tests/http.rs:4546` 断言 tail 输出逐字节相等。
- `docs/core-selection-spec.md:369-376` 已排除 JSON lines、远程 syslog、journald / Event Log 双写；同一节写的「1 MiB × 3 轮转」与 `platform/limits.rs:228` 的 64 KiB 默认值矛盾。

## Decision

- **行契约**：一行一条，固定为

  ```
  <RFC3339 UTC 毫秒> <LEVEL> <source> <message>
  ```

  `LEVEL` 定宽 5（`ERROR` / `WARN ` / `INFO ` / `DEBUG` / `TRACE`），`source` 取 `svcd`、`plugin:<id>`、`hook:<id>` 之一，其余是消息。消息内的 CR/LF 替换为字面 `\n`，保证「一行 = 一条记录」。不做 JSON lines。
- **级别**：五级，默认 `INFO`；配置键 `log_level`，大小写不敏感，非法值按配置错误硬错（与现有 parser 一致）。过滤在**写侧**：低于阈值的记录不落盘；读取侧的 `--level` 只做展示过滤，默认关闭，默认输出逐字节不变。
- **时间**：`LogSink` 持有组合根构造的 `Clock` 副本，每行写入时取一次墙钟毫秒；`Clock` 增加 `epoch_ms()`，`Now` 不变。这是对 ADR-0020 的两处修正：`Clock` 从「只有一个动作」变为带一个投影、`Clock` 变成 `Copy`。理由是日志时间戳属于 I/O 元数据，不参与任何状态判定；而让 109 处调用点各自传 `Now`，会把一个格式化细节变成全仓签名变更，插件读线程与钩子线程里也根本没有 `now`。
- **超长行**：`LogSink` 写入前把单行截到 8 KiB（按 UTF-8 边界）并追加 `…(truncated N bytes)`。reactor 的 64 KiB `MAX_LOG_LINE` 保留给帧与 raw；**日志源不再允许因长度被关闭**。
- **文件布局不变**：`svcd.log`、`<id>.log`、`<id>-hooks.log` 各自保留；「一次看全」由读取侧的 `svc logs --all` 合并排序，不改写入布局。
- **轮转**：`log_max_bytes` 配置优先，未配置时默认 64 KiB；`log_rotations` 默认 3。`core-selection-spec` §5.4 的 1 MiB 改为 64 KiB（配置仍可覆盖）。
- **消息里手写的 `svcd: ` 前缀删除**，由 `source` 列接替；插件原始行内容保持原样，不加前缀。

## Considered options

- **JSON lines**：解析免费，但每行多约 60 字节，而单文件阈值只有 64 KiB；`core-selection-spec` §5.4 也已排除。将来接采集器用 `log_format` 键加，不破坏现有解析。
- **读侧过滤**：证据完整，但 debug 会把 info 从 64 KiB 窗口里冲掉，实际可见证据反而更少，且磁盘无界。
- **109 处各传 `Now`**：最忠于 ADR-0020 的字面写法，但把日志时间变成所有调用点的参数，而负责写插件行的线程里没有 `now`。
- **超长行沿用「丢 source」**：实现最省，但这是静默丢失日志——最需要证据的时候证据消失。

## Consequences

- 两份文件有了共同时间轴，`svc logs --all` 能按毫秒合并；复盘不再靠猜顺序。
- 老日志文件没有时间戳与级别：读取侧必须容忍不匹配行契约的行，按原样显示、不参与 `--level` 过滤。
- `LogSink::line` 仍是「哑写一行」，原有单元测试断言不变；格式化、过滤与截断在 `LogWriter` 一层。
- 写侧过滤意味着 `DEBUG` 默认不落盘；要看 debug 必须改配置并重启。运行时改级别与写入背压不在本决定内；静态总开关是 `log = off`。
- 落地：`log_level` 与 `log` 进 `Settings`/`Limits`；`LogWriter` 负责时间戳、级别、来源、8 KiB 截断；插件与钩子管道共用它。
- 8 KiB 截断会丢掉单行超长内容的尾部；多行栈不受影响（每行独立），受影响的是把整个 JSON 打进一行的插件。
- 不在本轮：日志写入背压（同步 flush 卡循环）、syslog / Event Log 汇适配、审计日志、`log_level` 的 per-source 覆盖。
