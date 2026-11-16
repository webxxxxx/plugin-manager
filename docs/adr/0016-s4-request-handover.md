# S4 请求交接：服务器持有连接，对外面只解释请求

## Status

Accepted。细化 [ADR-0012](0012-protocol-is-server-plus-face.md)；切片见 #113，后续迁移见 #114–#125。

## Decision

- **连接只有一个所有者：服务器。** 服务器持有 fd、读缓冲、写缓冲、deadline、pending 表、keep-alive 与背压；对外面拿不到 socket、文件描述符、连接 guard，也拿不到连接上的超时。
- **S4 是 server ↔ face 的请求交接 seam**，形状是「路由决策 + 一次完成式回调」：
  - `Face::route(method, path) -> Option<Route>` 回答「这条请求归不归我、请求体怎么处理」。契约要求它在读取请求体之前被调用；本切片的旧引擎还没搬，仍在收完 body 后才问（#114 把调用提前）；
  - `Route { body, dispatch }` 是契约携带的路由/请求体决策。`body` 是 `Empty` / `Buffered { limit }` / `Streamed`；`dispatch` 是 `Inline` 或 `Blocking`；
  - `Face::handle(Request, Completion)` 只解释请求。`Request` 带 method、path、headers、peer、服务器解析好的 principal，以及按 body 策略给出的完整 body 或空 body。
- **交换只有两种结果**：`Exchange::Response(Response)`（status、content-type、headers、完整 body）或 `Exchange::Stream(Box<dyn StreamHandler>)`。`StreamHandler` 是 sans-IO 状态机：只收事件、返回动作，不读不写 socket；事件/动作面由 #115 落地。
- **完成恰好一次。** `Completion::complete` 第一次被接受；第二次被拒绝、忽略并记日志。`Dispatch::Inline` 的 handler 必须在 `handle` 返回前完成；返回时没有结果，服务器记 500（`internal`），封住完成槽，之后迟到的完成也被忽略。`Blocking` / 异步完成由后续切片的 server-loop 在 deadline 内等待。
- **结果必须与 route 的 body 策略一致。** `Streamed` 路由返回内存响应、或 `Buffered` / `Empty` 路由返回 stream handler，都是实现错误：服务器在发出响应头之前转成 500。
- **鉴权在服务器完成一次**：服务器把解析后的 principal 放进 `Request`；对外面只做端点策略（MCP bearer-only、插件 token、CSRF 等）。#113 的 `/api/v1/version` 没有额外端点策略。本切片仍沿用旧的每请求 `Lan::authenticate`；把 principal 提前到连接建立是 server-loop（#114）的后续工作。
- **本切片只让 `/api/v1/version` 穿过对外面**，其余 HTTP surface 仍走既有 worker / takeover 路径；没有新增每请求线程。两个引擎（Unix reactor、Windows 每连接线程回退）都在把请求交给旧路径之前先问 face。
- **请求体策略在本切片是「带着」而不是「提前用」**：旧引擎仍先按旧上限收完 body 再交给 face；#114 把 route 决策移到读 body 之前，#115 用 `Streamed` 把上传体交给 stream handler。`/api/v1/version` 是 `Empty`，不受影响。本切片没有 `Streamed` 路由；若出现，legacy 引擎明确返回 501（`not_implemented`），而不是假装把连接交出去。

## Consequences

- face 的测试不再需要 socket：内存 `Request` + `CompletionSlot` 就能断言响应、恰好一次、缺失完成和二次完成。
- 服务器可以替换（Unix poll reactor、Windows 线程回退、将来的 IOCP），face 与端点测试不变。
- `StreamHandler` 出现后，ADR-0012 原来说的「交出连接」只表示「交出一个 sans-IO 状态机」，不再表示连接所有权转移；连接始终归服务器。
- 迁移期会有两条路径并存；legacy 路径是临时的，#125 删除。
- 完成槽封住后迟到的完成只能记日志，不能写回一个已经结束的请求；这消除 use-after-response 的竞态。
