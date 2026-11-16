# S4 请求交接：服务器持有连接，对外面只解释请求

## Status

Accepted。细化 [ADR-0012](0012-protocol-is-server-plus-face.md)；切片见 #113，server-loop 已由 #114 接管 HTTP listener / 连接，阻塞池与首个 `Blocking` 路由已由 #116 接线，后续迁移见 #115–#125。

## Decision

- **连接只有一个所有者：服务器。** 服务器持有 fd、读缓冲、写缓冲、deadline、pending 表、keep-alive 与背压；对外面拿不到 socket、文件描述符、连接 guard，也拿不到连接上的超时。
- **Unix 的 server-loop 在 `server/http/engine.rs`。** 它拥有 listener、accept、连接表、请求缓冲、pending 请求与写回；未迁移的端点和流仍由 `api/legacy.rs` 的状态机处理（临时 legacy bridge）。bridge 的 HTTP 队列上限 64，满队列返回 503（带 `Retry-After: 1`）而不是增长；WS 任务不占这 64 槽，由连接数与 `ws_inflight_requests` / `ws_inflight_bytes` 约束。close/超时仍由 loop 统一移除连接。
- **阻塞池是 server 的固定资源（#116）。** 默认 2 条 worker，`blocking_threads` 可配且 clamp 2..8；队列上限 64，队列满或同一连接已有排队/运行中的任务时返回 503 `busy` + `Retry-After: 1`，不增长队列、不起临时线程。池在 server-loop 装配时启动（早于 loop 线程），启动失败会让 HTTP 启动失败，而不是静默停掉所有路由。每个被接受的任务只回报一次结局：`Finished`（运行完成，槽里可能有结果）、`Cancelled`（运行中连接断开，结果丢弃）、`Dropped`（排队任务被取消或池关闭，从未运行）；server loop 按结局取槽或回 503。`Processing` 连接每个 tick 用 `MSG_PEEK` 检查对端 FIN（不消费下一请求的字节），断开时封住完成槽、翻转运行任务的 cancellation token，排队任务立即丢弃。Unix 的 `/api/v1/services` 是第一个 `Blocking` 路由；Windows 的每连接线程回退在该连接线程上执行 `Blocking` 路由，不引入共享池。
- **S4 是 server ↔ face 的请求交接 seam**，形状是「路由决策 + 一次完成式回调」：
  - `Face::route(method, path) -> Option<Route>` 回答「这条请求归不归我、请求体怎么处理」。契约要求它在读取请求体之前被调用；#114 接管连接后仍在收完 body 后才问 face（`Blocking` 路由已接线，route 提前到读 body 之前仍待后续切片）；
  - `Route { body, dispatch }` 是契约携带的路由/请求体决策。`body` 是 `Empty` / `Buffered { limit }` / `Streamed`；`dispatch` 是 `Inline` 或 `Blocking`；
  - `Face::handle(Request, Completion)` 只解释请求。`Request` 带 method、path、headers、peer、服务器解析好的 principal，以及按 body 策略给出的完整 body 或空 body。
- **交换只有两种结果**：`Exchange::Response(Response)`（status、content-type、headers、完整 body）或 `Exchange::Stream(Box<dyn StreamHandler>)`。`StreamHandler` 是 sans-IO 状态机：只收事件、返回动作，不读不写 socket；事件/动作面由 #115 落地。
- **完成恰好一次。** `Completion::complete` 第一次被接受；第二次被拒绝、忽略并记日志。`Dispatch::Inline` 的 handler 必须在 `handle` 返回前完成；返回时没有结果，服务器记 500（`internal`），封住完成槽，之后迟到的完成也被忽略。`Blocking` 路由由阻塞池执行，完成回到 server loop 写回；等待 deadline 的通用机制仍待后续切片（`svc.list` 的 job 自身没有额外 deadline）。
- **结果必须与 route 的 body 策略一致。** `Streamed` 路由返回内存响应、或 `Buffered` / `Empty` 路由返回 stream handler，都是实现错误：服务器在发出响应头之前转成 500。
- **鉴权在服务器完成一次**：服务器把解析后的 principal 放进 `Request`；对外面只做端点策略（MCP bearer-only、插件 token、CSRF 等）。#113 的 `/api/v1/version` 没有额外端点策略。#114 仍沿用旧的每请求 `Lan::authenticate`；把 principal 提前到连接建立是后续 server-loop 工作。
- **#113 只让 `/api/v1/version` 穿过对外面**；#114 让 server-loop 接管全部 HTTP 连接；#116 让 `/api/v1/services` 走 `Blocking` 并新增固定 2 条阻塞线程。其余端点仍经有界 legacy bridge 走既有 worker / takeover 路径。没有新增每请求或每流线程；Unix 迁移期默认从 10 条升到 12 条（2 条阻塞线程；4 条 legacy worker 等 #125 删除），Windows 仍保留每连接线程回退。
- **请求体策略仍是「带着」而不是「提前用」**：server-loop 在 `pre_request` 里先给 legacy takeover 检查原始 head（上传/安装/导出仍能流式接手），其余请求仍按旧上限收完 body 再交给 face。把 face 的 route 决策移到读 body 之前仍待 #117/后续切片；`Streamed` 事件面由 #115 接通。本切片没有 `Streamed` 路由；若出现，legacy 引擎明确返回 501（`not_implemented`）。

## Consequences

- face 的测试不再需要 socket：内存 `Request` + `CompletionSlot` 就能断言响应、恰好一次、缺失完成和二次完成。
- 服务器可以替换（Unix poll reactor、Windows 线程回退、将来的 IOCP），face 与端点测试不变。
- `StreamHandler` 出现后，ADR-0012 原来说的「交出连接」只表示「交出一个 sans-IO 状态机」，不再表示连接所有权转移；连接始终归服务器。
- 迁移期会有两条路径并存；legacy bridge 是临时的，#125 删除。HTTP 队列有界（64），阻塞池队列也有界（64），满时 503；池关闭时排队任务以 `Dropped` 回报（server loop 若还在运行会回 503），运行中的任务让它跑完并把结果写入已封的槽后丢弃。WS 任务由连接数与流内限额约束。五个 takeover 仍是迁移期每请求一线程（既有行为，随 #118–#122 删除），不计入这里的队列；连接在 handover 期间移出 loop 的表，`max_connections` 对这段窗口不再生效。这是 #114 之前就有的迁移缺口，不在本切片改变语义。
- 本 spec 的最终线程目标（#126 合并插件 stdio 反应器后：「两条循环 + 两条阻塞线程，默认 4 条」）是 Unix 的默认拓扑。Windows 本轮保留每连接一条线程的回退：它执行 `max_connections` / `max_connections_per_ip` / `http_idle_timeout_secs`，线程上限由 `max_connections` 约束，但不承诺 4 条线程；IOCP 留给 S3/S6。
- 完成槽封住后迟到的完成只能记日志，不能写回一个已经结束的请求；这消除 use-after-response 的竞态。
- `Blocking` 任务在派发前检查 cancellation token；`svc.list` 的 control-channel 往返本身没有可中断 deadline，连接断开后结果会被丢弃，但一个卡死的 daemon 仍会占住一个 worker。给 job 加自身 deadline 留在后续切片；阻塞池的队列上限保证不会因此无限增长。
