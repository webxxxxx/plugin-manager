# Windows 也跑同一个 server loop：WSAPoll 后端

## Status

Accepted（2026-11-22）。细化 [docs/architecture.md](../architecture.md) §8 的 S3；
连接层的 Windows 反应器。

## Context

- Windows 原来是「每连接一条线程」（`server/http/thread_loop.rs`）。线程栈进内存不变量
  （`request_stack_bytes × max_connections`），所以同一个预算下 Windows 的连接数远低于
  Linux；[2026-11-21 吞吐矩阵](../perf/2026-11-21-throughput-after.md) 的下一步就是
  「Windows 端把线程栈换成 reactor」。
- server loop 本身与平台无关：`server/http/engine.rs` 拥有连接状态机、`server.rs` 拥有
  face/pending/blocking 完成、`server/http/driver.rs` 拥有 stream 背压与 `max_streams`。
  它只在三处接触宿主：等待原语（`poll(2)`）、唤醒管道（`UnixStream`）、控制通道
  （Unix domain socket）。
- Windows 的命名管道不是 socket，不能进 `WSAPoll`；控制通道本来就在自己的命名管道
  accept 线程上，MCP SSE 需要一条去控制通道的出站流。

## Decision

- **一个 poll seam，两个后端**：`platform/host/poll` 暴露中性的 `PollFd` 与事件位
  （`READ` / `WRITE` / `ERROR` / `HUP` / `RDHUP`、`CLOSE` 组合、`NONE` 槽）。
  Unix 后端是 `poll(2)`，Windows 后端是 `WSAPoll`；后端只编译进各自宿主。
- **engine/server/driver 去平台分叉**：连接状态机、stream driver、预算、out 缓冲背压、
  blocking pool、deadline 只有一份代码，两端跑同一份逻辑。
- **唤醒管道**：`platform/host/wake_pipe`。Unix 用 `socketpair`；Windows 没有
  `socketpair`，用两条连通的 loopback TCP socket，两端非阻塞。写侧是 `Arc<Writer>`，
  读侧只有 loop 自己持有。
- **控制通道不并入 Windows wait set**：Windows 保留命名管道那种「每连接一条线程」的
  accept/读路径（现状）。`StartInput.control` 在 Windows 只是一个 marker，HTTP loop
  没有 control extras；Unix 仍把控制 listener/连接并进同一个 wait set。
- **MCP SSE 在 Windows 交接给一条线程**：命名管道流没有 wait-set 集成，所以把连接从
  loop 里交出去，跑现有的 blocking fallback（`server/mcp/fallback.rs`）。线程数受
  `max_streams` 约束，预算、`transfer_timeout`、断开探测与它在线程驱动上的语义一致；
  不另写一份 SSE 协议。
- **线程记账跟随拓扑**：连接是 wait set 条目不是线程，所以删掉
  `stack_per_connection`，`Limits::threads()` 两端都是 loop 线程 + blocking 线程；
  同一预算、同一 `ResourceProfile` 下 Windows 与 Linux 派生出的连接数相同。
- **插件 stdio 反应器不在本 ADR 内**：它的 Windows 后端由
  [ADR-0028](0028-windows-plugin-stdio-reactor.md) 补齐（overlapped 命名管道 + IOCP），
  Windows 上同样零线程，`/raw` 也不再回 501。

## Considered options

- **IOCP proactor**：Windows 原生的规模化方案，但 engine 的契约是 readiness（读一次、
  `WouldBlock` 即空），换 completion 就要把每连接缓冲、读写状态与 `driver.rs` 的
  `blocked_on_write` 全改一遍，等于重写 S4。当前连接上限是 786（2 GB 预算）、
  `WSAPoll` 每次 O(n) 足够；等连接数级别再谈。
- **保留线程驱动，只放宽连接记账**：不减少每连接的线程栈 RSS 与调度成本，也没有解决
  c50/c100 时 500–1000 条线程的问题，与 perf 矩阵的测量直接矛盾。
- **Windows 控制通道改用 AF_UNIX 并进 wait set**：要绕开 std 自建 socket + ACL，
  收益只在控制连接很密的场景，本次不做。

## Consequences

- Windows 与 Unix 的连接状态机、背压与失败类别只有一份实现；新增端点不需要考虑宿主。
- Windows 空载线程仍是 4 条（supervisor、server loop、两条阻塞线程）+ blocking pool；
  控制连接和 MCP SSE 交接是唯一还会按连接起线程的地方。
- 非 Windows 构建不包含任何 Windows 后端代码（`poll/windows.rs`、`wake_pipe` 的
  Windows 分支都在 `cfg(windows)` 下）；Unix 侧只是把原来的 `libc::poll` 调用换成
  同一个 seam 的 Unix 后端。
- Windows 多一对 loopback socket（唤醒管道），进程退出时随 `LoopHandle` 一起释放。
- 行为差异只剩：Windows 不报告半关闭（`POLLRDHUP`），整个连接关闭会以 `POLLHUP`
  报告（`poll/windows.rs` 有测试钉住），所以暂停中的 stream 仍能在客户端断开时取消。
- 测试：poll 后端（可读、超时、零 interest 下的 close）、wake pipe、连接行为由
  `tests/http` 在两端整跑。
- 实测（Win11，debug 构建）：空载 15 条线程；建立 200 条 TCP 连接后仍是 15 条，
  连接全部关闭后仍是 15 条——改造前会按连接多出 200 条线程。
