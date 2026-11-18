# 投影是面中立的值：Status 与 Live 进内核

## Status

Accepted（2026-11-18）。细化 [docs/architecture.md](../architecture.md) §5 的 `kernel/projection.rs` 行，并替换 §7 的反例表。实施切片：#209（删死接线）、#211（envelope 落面）、#212（类型搬家）、#213（文档与词表）。与 [ADR-0011](0011-service-contract-is-the-only-composition-point.md)（契约是唯一组合点）、[ADR-0019](0019-config-is-derived-once-at-the-composition-root.md)（配置是只读值，传值不传 trait）、[ADR-0020](0020-time-is-a-parameter-not-a-trait.md)（时刻成对，此后只做纯换算）同向：投影也是一个只读值加纯函数，不新开 seam。

## Context

- `docs/architecture.md` §5 从 #170 起就写着 `kernel/projection.rs`（「能力面 → 各对外面的唯一投影源」），但文件至今不存在。
- **状态投影的载体类型在 L5。** `Status` / `Summary` / `Details` 曾定义在 `src/cli/status.rs`；组合根反向引用它——`daemon.rs` 里的 `use crate::cli::status::Status`。而 `Status` 除 `supervise::model::supervisor::Machine`（L3）之外，依赖全是 `kernel::registry::{Kind, Service, State}` 与 `platform::clock::Now`。
- **「唯一投影源」在值的层面今天已经成立。** 三个对外面都是控制通道 RPC 客户端，不是各自投影：CLI 的 `svc.list` 把结果 `serde_json::from_value::<Vec<Status>>`；HTTP 面走 `face/api.rs` 的 `rpc()` → `face/support.rs` 的 `rpc_result` → `channel::request`；MCP 桥走 `face/mcp.rs` → `channel::request("registry.tools")`。缺的不是合并重复，而是这个值的**类型没有合法落点**。
- **「投影族」其实是三族黏在一起**（`daemon.rs` 约 350 行）：值形状构造（真正的实现在 `cli/status.rs` 的 `of` / `live` / `builtin`）、MCP envelope（`project_slot_response` / `_outcome`、`project_rejection`、`project_builtin_outcome`）、会话状态（`catalog_version` / `catalog_cache` / `tools_page`）。
- **层 rank 允许 envelope 进内核，语义不允许。** `tools/arch-map.json` 里 platform 0 < wire 1 < kernel 2 < domain 3；`rpc::Response` 属 wire（rank 1），内核依赖它不违规（1 ≤ 2）——但那等于让内核认识 JSON-RPC。
- **有一条现成的对称落点。** `problem_data`（`daemon.rs`）写 `{"kind": …}`，而对称的解码器 `Problem::from_error_data` 已在 `kernel/failure.rs`，被 `face/support.rs` 依赖。
- **§7 的三条「真实反例」全部过时**（`node tools/dep-lint.mjs` = 0 违规）：`slot` 调 `mcp::tool_error` 已不存在（`grep -rn "crate::face" src/supervise src/manage src/kernel src/platform` 为空）；`transfer`（`manage/ops/transfer.rs`）已不依赖 `status`；`install` 调 `daemon::shutdown_quiet` 只剩 `main.rs` 里同一 rank 的调用。
- **同一转换有两处实现。** `Status::live` 的 `epoch_secs` 带 `ms == 0 → None` 护栏；`daemon.rs` 的 `persist_states` 自己算一遍，没有护栏（#215 收掉写侧；读侧的缺失回落见 #216）。
- **「逐字节不变」这条验收今天不成立。** 没有任何测试逐字节比对 HTTP / MCP 的 JSON 响应，全是按 `Value` 键值断言；`serde_json` 未开 `preserve_order`（`Cargo.lock` 里没有 `indexmap`），所以线上 `Map` 是 `BTreeMap`（键按字母序），而 CLI `svc status --json` 走结构体声明序。

> 本节的 `file:line` 口径：只写符号与文件名，不写行号——这些位置在 #209 / #210 / #211 / #212 落地后都动过。落地后的落点见下面这条。
>
> - `Status` / `Live` → `kernel/projection.rs`（#212）；`cli/status.rs` 只剩 `table` / `text`。
> - MCP envelope 四函数 → `face/mcp_tools.rs`（#211）。
> - `problem_data` → `Problem::error_data()`（`kernel/failure.rs`，#212）。
> - `Slot::set_projector` 连同 `CallSink` 枚举 → 已删（#209）。
> - 分页包改成控制通道传面中立的 `next`、MCP 面渲染 `nextCursor`；auth 四键收敛到 `LanStatus`（#210）。

## Decision

- **投影两分。** **值投影** = 领域值 → 面中立值形状的**纯函数**，归内核；**协议投影** = 面中立值 → 某协议报文，归该对外面。判据只有一条：输出里有没有某个协议的词（`inputSchema`、`nextCursor`、JSON-RPC 帧、HTTP 状态码）。
- **第三类排除项：面级装饰。** `hasConfig` / `hasUI`（`face/api.rs`）这类字段可以在投影值之上追加，也可以读盘（`manage::plugin::web_assets`），但它不是投影，不进内核形状。
- **`kernel/projection.rs` 的内容。** `Status` / `Summary` / `Details` + `Live` + `of` / `live` / `builtin` 与 `anchored_secs*`（#215 起 `pub(crate)`，写侧也调它）。只依赖 `kernel` 与 `platform`；不出现进程、socket、文件。
- **`Live` 是 `Machine` 被投影读到的那 9 个字段。** 六态、pid、失败次数、`since_ms`、`ready_since_ms`、`next_at_ms`、最近退出码、不可杀标记、重启策略。时刻**只存单调毫秒**；`impl Machine { pub fn live(&self) -> Live }` 是纯字段拷贝、**不接收 `Now`**——锚定留在 `Status::live(service, &live, now)` 一处，跨层边界不传时间。`remain_after_exit` / `fail_after_stop` / `degraded` 不进：今天没有任何对外面读到它们。
- **`problem_data` 进 `kernel/failure.rs`。** 它写 `{"kind": …}`，与已有的解码器对称；帧构造（`rpc::err_data`）仍留在面。
- **MCP envelope 四函数进 `face/mcp_tools.rs`。** 组合根只做装配与分发；`face/mcp.rs` 保留桥客户端（`run_stdio` / `Bridge` / era 装饰）。这与 §5 早已写下的文件名对齐。
- **分页与缓存判为协议加会话状态。** `tools_page` / `catalog_version` / `catalog_cache` 位置不动：cursor 对客户端不透明、按版本校验快照、上限常量在 wire（`CATALOG_PAGE_MAX`），三个特征都是协议。
- **能力面 → tools 不进内核。** 它只有 MCP 一个消费方（HTTP 没有能力面路由），按 §8 的 seam 判定不建「内核值 + 面映射」两层；`tools()` 留在 `face/mcp_tools.rs`。
- **不新开 trait。** 投影不做成端口：只有一个实现，且 ADR-0019 已为配置拒过同一形状。
- **验收三档。** ① CLI 文本逐字节不变；② 控制通道 / HTTP / MCP 的 JSON 键集合、值与嵌套不变，**键序不承诺**；③ 跨面一致性断言（stdio 与 HTTP 的工具名相同等）保留。
- **§7 反例表换新，并保持一条未修。** 表的作用是证明四条不变量不是空话；全换成已修的就又成了空话。

## Considered options

- **投影是端口（内核定 trait，L3 实现）。** 接口漂亮，但今天只有一个实现，且投影是纯数据、不需要动态分发；ADR-0019 已经为配置拒过同一形状。按 §8「一个实现 = 想象中的 seam」否掉。
- **`kernel/projection.rs` 只收能力面（`tools` 族）。** 最小步子，但 §5 的「唯一投影源」只兑现一半，还把 MCP 的协议词（`inputSchema`）放进内核，与值投影的定义冲突。
- **整条链进内核（值 + envelope）。** rank 允许，语义不允许（见 Context）；要让内核定「响应契约」得重排层，收益不足。
- **不搬，删掉 §5 的 `projection.rs` 这一行。** 诚实（承认投影归 daemon），但 `Status` 会继续由 L5 定义、被 L6 引用，ADR-0011 的「契约是唯一组合点」在投影这一侧落不了地。
- **`Live` 直接存 epoch 秒（L3 里转换）。** 少一次内核内锚定，但 `Now` 会越过层边界进 L3 的转换函数，与 ADR-0020 的「此后只做纯换算」拧着。
- **给 `Status` 加 `hasConfig` / `hasUI`。** 让内核读盘，违反纯函数口径，还要改 HTTP 列表契约。

## Consequences

- `daemon.rs` 不再 `use crate::cli::status`；组合根持对外面类型的反例消失（#212）。
- `Slot::set_projector` 曾是「想象中的 seam」的现成反例：唯一注入点在组合根，而读它的 `Slot::project` 与 `CallSink::Client` 都标着 `#[cfg(test)]`，生产路径只走 `CallSink::Completion` → `projected_done`。**已由 #209 把那套机制连同 `CallSink` 枚举一起删掉**：槽的完成回调现在直接就是调用方给的 `ReplySink`。
- `persist_states` 的重复换算**已由 #215 收掉**（写侧行为改动，带模块级测试）：它改吃 `Machine::live()`，锚定只走内核的 `anchored_secs`，`since_ms == 0` 不再落盘成伪造的 epoch 秒。`adopt_runtime` 在记录缺 `since` 时回落 `now` 是读侧的同型漏网，另开 **#216**。
- `face/mcp_tools.rs` 与 `kernel/projection.rs` 已登记进 `tools/arch-map.json`（#211 / #212）。
- 词表新增「值投影」「协议投影」「运行面（Live）」三条；前两条由 #210 落地，第三条由 #213 补。
- MCP 分页包与 auth 四键的重复**已修于 #210**：控制通道返回面中立值（分页用 `next`），由 MCP 面渲染 `nextCursor`；auth 四键从同一 `LanStatus` 渲染，并有一条跨面一致性测试。#213 负责把 §7 的这条反例换掉。
- **控制通道只传面中立值。** 控制 RPC 的结果不写对外协议的字段名；需要跨进程传递的值用中性名，由对外面渲染成协议报文。
- **对外协议的报文只在面里组装。** 组合根只装配与分发，不写对外协议的键名。
- **同形承诺要有跨面测试。** 两个面对外承诺同一形状（如 auth 四键）时，值只能来自一个面中立值；各自渲染时，必须有一条跨面一致性测试，并证明编码器被人为改坏时它会红。
