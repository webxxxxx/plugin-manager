# Windows 插件 stdio 也进反应器：overlapped 命名管道 + IOCP

## Status

Accepted（2026-11-22）。补齐 [ADR-0027](0027-windows-runs-the-same-poll-loop.md)
留下的插件 stdio 半边；对应 [docs/architecture.md](../architecture.md) §8 的 S3/S6。

## Context

- Unix 上插件 stdio 由 supervisor loop 的 poll 集拥有：每个插件不再自带读写线程，
  帧/日志解析与有界队列只有一份，见 [GLOSSARY](../GLOSSARY.md) 的「插件 stdio 反应器」。
- Windows 一直是 `CreatePipe` + 每管道读写线程：匿名管道不能 overlapped，也不能进
  `WSAPoll`，所以 reactor 无法直接接管。
- 目标与 Linux 相同：两个 reactor（server loop + supervisor 的插件 stdio），阻塞工作进
  blocking pool，**插件数量不增加 daemon 线程**；背压语义（有界帧队列、raw 写队列、
  停止原因）保持一份实现。

## Decision

- **父端换成 overlapped 命名管道**：`stdio/platform/windows.rs` 用
  `CreateNamedPipeW(PIPE_ACCESS_INBOUND/OUTBOUND | FILE_FLAG_OVERLAPPED)` 建父端，用
  `CreateFileW` 在进程内建子端，再交给 `Stdio`（`Command::spawn` 仍只把子端复制成
  可继承）。方向与匿名管道一致：stdout/stderr 父读、stdin 父写。Windows 上不再有
  匿名管道：反应器没起来时的线程回退拿到的也是同一对命名管道句柄，只是不走
  overlapped 语义。
- **IOCP 后端**（`reactor/iocp.rs`）：每个 Source 最多一个 pending read 和一个 pending
  write；`OVERLAPPED` 放在堆上的 slot 里，slot 地址就是 completion key；
  `Reactor::poll_once` 就是 `GetQueuedCompletionStatusEx`，中断是
  `PostQueuedCompletionStatus`（key 0）。反应器没有自己的线程，supervisor loop 驱动它。
- **背压沿用 Linux 的语义**：读完成才解析；解析遇到有界的 `FrameEvent` 队列满就
  `blocked`，不再提交下一次读（内核管道缓冲随后填满，插件 write 自然阻塞）。raw stdin
  从 `RawOutQueue` 取当前切片，完成后再取下一段，队列容量仍是 `writer_queue`。
- **停止与取消**：`CancelIoEx` 后等 pending 计数归零才释放 slot（完成包还指向它的
  `OVERLAPPED`）；一次插件停止只向 hub 报一次 `RawFailure`。
- **seam 不再按平台分叉**：`Target::polled`、`Reactor`、`RawHub` 的写队列在 Windows
  全部启用；`/raw` UI 路由在 Windows 从 `501 not_supported` 变成正常服务（协议 3
  插件 + 反应器）。
- 线程预算不变：[ADR-0027](0027-windows-runs-the-same-poll-loop.md) 已去掉连接线程栈，
  插件 stdio 这里不新增线程。

## Considered options

- **保留每管道 reader/writer 线程（现状）**：不满足「每个插件不会新线程」，也不满足
  与 Linux 相同拓扑。
- **`RegisterWaitForSingleObject` + PeekNamedPipe 读、阻塞写进池**：读可行，但慢插件的
  stdin 写会长期占住 blocking pool 的 worker，把管理请求饿死；写没有就绪通知，只能靠池
  阻塞。
- **匿名管道 + 每 daemon 一条 writer 线程**：一条满管道会阻塞整条 writer，等于把所有
  插件串行化。
- **子进程 stdio 换 AF_UNIX socket 对（可 `WSAPoll`）**：要自建 socketpair、继承
  SOCKET 并绕开 std 的 `CloseHandle` 语义，组合没有先例，风险高于命名管道 + IOCP。

## Consequences

- Windows 与 Linux 只差后端：帧/日志解析、有界队列、停止原因、raw 帧路由是同一份代码。
- Windows 空载仍是 4 条线程；N 个插件只增加 pending I/O（每管道一个读，raw stdin 一个
  写），不增加线程。
- 子进程看到的是命名管道而不是匿名管道：同样是 `FILE_TYPE_PIPE`、阻塞读写与 EOF
  语义；插件生命周期、日志、MCP 与 raw 集成测试在 Windows 整跑。
- 取消路径必须等完成包回收 slot；不能在 `CancelIoEx` 后立即关闭句柄。
- 实测（Win11，debug，50 MB 预算、5 个 notify 插件、10 条管道）：daemon 空载 10 条
  线程，5 个插件跑起来后 11 条；改造前会按管道多出约 10 条。Linux 全量与 Windows
  全量（762 单元 + 87 HTTP 等）均通过。
- 非 Windows 构建不含任何 IOCP/命名管道代码；Unix 侧只把 `read_stream`/`read_raw` 拆成
  「字节到达后怎么处理」，readiness 驱动不变。
