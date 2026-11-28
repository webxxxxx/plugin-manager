# 调用号：一次请求一个号，错误与日志各说一遍

## Status

Accepted（2026-11-28）。落地 #343。

## Context

- 安装失败时用户只有一句 `post-install hook failed: exit status 3`。`svcd.log` 的安装行只有插件 id 与版本，`<id>-hooks.log` 是历次钩子输出拼在一起、中间没有分隔；同一个插件装三遍，分不清哪几行属于哪一遍，也分不清那次 `exit 3` 对应哪一遍。
- 能力调用在槽里有唯一号（`PendingCalls::allocate()`，也是发给插件的 JSON-RPC `id`），但超时行只写能力名，不写这个号——插件那侧看到的是 `id`，svcd 这侧写的是名字，两边对不上。
- 控制通道的 JSON-RPC `id` 由客户端写死成 `1`（`wire/control/channel.rs` 的 `request_line`），它只用于配对，不承担身份；HTTP 面又把 `error.data` 整个丢掉。
- 结论：svcd 内部没有任何东西能命名「这一次请求」。

## Decision

- **一个分配点**：`Daemon::handle`，一条 `PendingCall` 一个号。所有入口（CLI、`/rpc`，以及 HTTP 面背后的控制通道）都经过它，所以不会漏也不会重复计数。一次请求内部的能力调用共用这个号。
- **从 1 起、重启归零**：纯内存序号，不落盘、不进配置、不做跨重启唯一。`starting` 日志行是归属边界，64 KiB × 3 的日志窗口不会长过一个 boot。
- **回传只走错误**：`error.data` 增加 `call` 键（`{"call":7,"kind":"internal"}`），成功响应不加任何键。号的作用是给失败命名；给结果加元数据会改动每个消费者的形状。
- **不写进 `error.message`**：消息是给人读的一句话，机器字段进结构化负载；CLI 自己拼 `(call=7)`。
- **日志用 `call=7` 开头**：安装成功行、安装失败行、慢请求行。成功行沿用 `install_log_line` 的输出，不把它改成带前缀的函数。
- **槽内调用号写进超时行**：`tools/call #7 <capability> timed out`。这里的号是发给插件的 JSON-RPC `id`，与调用号不是同一个东西，所以用 `#` 而不是 `call=`。

## Considered options

- **让客户端把 JSON-RPC `id` 变成唯一**：CLI、MCP、HTTP 面各发各的，号与 svcd 的日志没有约定；而且 HTTP 面根本没有 JSON-RPC `id`。
- **顶层响应加 `call` 字段**：JSON-RPC 客户端会忽略未知字段，但 MCP 与 HTTP 的响应形状是钉过的；放进 `error.data` 只动错误路径，向后兼容。
- **把号拼进 `error.message`**：省掉所有消费方的改动，但消息从此带上机器字段，读起来更差，也让按消息做的断言失效。
- **线程局部的「当前调用」（span）**：能自动给所有日志行加号，但把隐式状态引进一个到处显式传参的代码库；槽里的日志还发生在另一个 tick，线程局部早就没了。

## Consequences

- 失败的安装可以在 `svcd.log` 里按号读全：`call=3 install failed:`，以及同一号的成功行、慢请求行。
- 重启后号会重复，`starting` 行是分界。
- HTTP 面把号放在失败响应的 `X-Request-Id` 头里（#346）：body 形状不动，`error.data` 仍然不进 HTTP 错误体。
- MCP `tools/call` 的失败在 `_meta["io.svcd/call"]` 带号（`isError` 结果）；JSON-RPC 错误沿用 `error.data.call`，modern 收尾改为合并 `_meta` 而不是替换（#347）。
- 号同时填进 `Invocation::trace`，所以插件的超时行（`call=7 demo: tools/call #3 read timed out`）与 hook 日志的起始行也能带同一个号；插件帧里看不到它（#348）。
- 号不持久化，所以它不能当幂等键，也不能用来轮询一个已经结束的安装。
