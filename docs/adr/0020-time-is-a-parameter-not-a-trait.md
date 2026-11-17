# S8 时钟：时间是参数，不是 trait

## Status

Accepted。细化 [docs/architecture.md](../architecture.md) §8 的 S8 行；切片 S8a（`Now` / `Clock` + 循环拆壳）与 S8b（cadence 收口）已交付（#160）。与 [ADR-0019](0019-config-is-derived-once-at-the-composition-root.md)（配置在组合根派生一次）同向：配置与时间都只在组合根产生，传值不传 trait。

## Context

- 监督层的两个核**已经是**「时间作参数」：`supervisor::Machine::step(event, now_ms)`（`src/supervisor.rs:123`）、`Slot::{pump, apply}(…, now)`（`src/slot.rs:484/520`）。`supervisor.rs` 与 `slot.rs` 的生产代码里没有一处时钟读取；`slot.rs` 的测试已经在用 `pump(0)` / `pump(10_200)` 驱动。
- 剩下的时钟读取全在**壳与投影**上：`started = Instant::now()` 与 `epoch_at_start = SystemTime::now()`（`src/daemon.rs:581/582`）、`now_epoch_secs()`（`src/daemon.rs:285`），以及每次换算都要再读一次墙钟的 `epoch_of` / `ms_of`（`src/daemon.rs:607/614`）。`Status::live(service, machine, epoch_at_start)`（`src/status.rs:83`）因此有三个互不相干的基准。
- 后果：`runtime` 记录存 epoch 秒、监督内部跑单调毫秒（`src/runtime.rs:8` 自己的注释写着这件事），同一逻辑时刻可能被赋予两个 epoch；跨秒时 `since` 漂一秒，时钟回拨与午夜边界靠 `saturating_sub` 吞掉。
- 循环壳的定时器（目录重扫、临时文件清理、`START_TIMEOUT_MS`、`SHUTDOWN_BUDGET_MS`、接管宽限）没有任何单测：`Daemon::event_loop` 只在 `src/daemon.rs:246` 被调一次。测试只能起真 daemon 再 `sleep`，最贵的是 `tests/daemon.rs:2628` 为跨过 `STABLE_MS` 睡的 11 秒。
- cadence 是一把旋钮两个来源：`SVCD_RESCAN_INTERVAL_MS` 与 `SVCD_BOOT_SCAN_WAIT_MS` 仍在模块里直接读进程 env（`src/daemon.rs:543/328`），而 `temp_cleanup_interval` 早已来自 `Settings`。

## Decision

- **时刻是一个成对的值**：`Now { mono_ms: u64, epoch_secs: u64 }`，`Copy`。单调与墙钟由同一次读取捕获，此后 `epoch_of` / `mono_of` 是 `Now` 上的纯函数。`SystemTime` 不出现在监督层。
- **时钟是生产侧唯一的产出点**：`Clock::boot()` 在 `daemon::run` 里构造一次（`origin = Instant::now()`、`epoch_origin = SystemTime::now()`），只有 `now() -> Now` 一个动作。它既不是 `Config` 的字段（`Config` 是 ADR-0019 定死的四字段只读快照），也不是 `Daemon` 的字段。
- **传参数，不传 trait**。`Clock` 以参数进入 `event_loop(&mut self, clock: &Clock)`；`Daemon` 的所有时间入口收 `now: Now`。不引入 `trait Clock`、不做可推进的假时钟、不测阻塞本身。判据是既有事实：`Machine` / `Slot` 早就这么写，`slot.rs` 的测试已经用合成时间戳驱动。
- **循环壳拆成两段**：`tick(now)`（drain + pump + 重扫/清理到期 + 关机判定）与 `next_wait(now) -> Duration`（下一个截止时刻减去 now，`POLL_WAIT` 作上界）；`event_loop` 只留「算多久 → 等 → 回到 tick」。两段都能直接调，不睡不推进。
- **缝的边界一句话**：监督循环里出现的时刻只有一个来源；其它地方的 `Instant` 都是「等待」。显式留在缝外：hook 超时、interpreter 版本探测、status hook 探测、SCM readiness、blob 的 GC cutoff 与 mtime、`plugin::exported_at`、`server/http/**` 与 `lan` 的 deadline 与心跳。
- **cadence 收口**：新增 `rescan_interval_secs` / `boot_scan_wait_secs` 两个 `Settings` 键（秒；`0` = 关闭），写进 `svcd.conf.example`；`SVCD_RESCAN_INTERVAL_MS` / `SVCD_BOOT_SCAN_WAIT_MS` 保留为 bootstrap 阶段的覆盖（毫秒、冻结不再扩展）。删除 `SVCD_*` 覆盖仍是 ADR-0019 留给另一张票的事。

## Slices

- **S8a**：`Now` + `Clock` + `tick` / `next_wait` 拆壳 + 迁移 `Status::live`、`persist_states`、`adopt_runtime`；删 `now_epoch_secs`、`epoch_of`、`ms_of`；用模块级 `tick` 测试替换 `tests/daemon.rs:2628` 的 11 秒。
- **S8b**：两个 `Settings` 键 + env 覆盖 + `svcd.conf.example` + ADR-0005 / `GLOSSARY.md` 里过时的 env 引用。

## Considered options

- **可推进的 `Clock` 值**（`Arc<AtomicU64>` 偏移）：能让测试驱动真实 `event_loop`，代价是生产路径多一次原子读、以及「为什么生产要有 offset」这个解释；并且与「时间作参数」的既有纪律重复。
- **`trait Time { now, wait_until }` + 两个实现**：唯一能覆盖阻塞本身的做法，也是唯一与 ADR-0019 正面冲突的做法。

## Consequences

- 同一逻辑时刻只有一个 epoch；`Status`、`runtime`、`adopt` 共用同一个锚。
- 循环定时器可确定性单测，`tests/daemon.rs` 不再需要为跨过 `STABLE_MS` 睡 11 秒。
- 时间读取点从壳里的十几处收成 `Clock::boot()` 一处；新增定时器必须先算出截止时刻（`next_wait`），否则编译不过。
- 跨进程的定时器行为仍只有集成测试覆盖；S8 不改测试进程与 daemon 子进程的边界。
- 不在本决定范围：删 `SVCD_*` 覆盖、协议侧 deadline 的统一、`log` 注入、目录重排（step 6）。

## Clarification (S8a / S8b, #160)

- **`drain` 在 `tick` 之外。** 决定里写的「`tick(now)` = drain + pump + 定时器」在实现时拆成了 `drain(clock)` 与 `tick(now)`：请求处理会走阻塞的 start / stop / reload，需要时钟，而 `tick` 只拿固定的一对 `Now`。`event_loop` 因此是 `drain → clock.now() → tick → next_wait → wait`。定时器仍只读 `tick` 的那一对值。
- **`tick` 也多收一个 `&Clock`。** 唯一原因是到期的 rescan 会经 reconcile 走阻塞 start / stop；它不参与「现在几点」的计算。测试传 `Clock::boot()`，驱动定时器的是 `now` 参数。
- **两个 cadence 键用毫秒**（`rescan_interval_ms` / `boot_scan_wait_ms`），不是决定里写的秒：`SVCD_*` 覆盖本来就是毫秒，测试依赖 50ms 的 rescan；同一旋钮两个单位会更容易错。`0` = 关闭。
- **env 覆盖的最后位置是 `Config::bootstrap`**（`settings::apply_loop_env_overrides`），`settings::load` 保持纯解析。
