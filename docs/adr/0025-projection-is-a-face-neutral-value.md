# 投影是面中立的值：Status 与 Live 进内核

## Status

Accepted（2026-11-18）。细化 [docs/architecture.md](../architecture.md) §5 的 `kernel/projection.rs` 行，并替换 §7 的反例表。实施切片：#209（删死接线）、#211（envelope 落面）、#212（类型搬家）、#213（文档与词表）。与 [ADR-0011](0011-service-contract-is-the-only-composition-point.md)（契约是唯一组合点）、[ADR-0019](0019-config-is-derived-once-at-the-composition-root.md)（配置是只读值，传值不传 trait）、[ADR-0020](0020-time-is-a-parameter-not-a-trait.md)（时刻成对，此后只做纯换算）同向：投影也是一个只读值加纯函数，不新开 seam。

## Context

- `docs/architecture.md` §5 从 #170 起就写着 `kernel/projection.rs`（「能力面 → 各对外面的唯一投影源」），但文件至今不存在。
- **状态投影的载体类型在 L5。** `Status` / `Summary` / `Details` 定义在 `src/cli/status.rs:19/25/36`；组合根反向引用它——`daemon.rs:20` 的 `use crate::cli::status::Status`。而 `Status` 除 `supervise::model::supervisor::Machine`（L3）之外，依赖全是 `kernel::registry::{Kind, Service, State}`（`cli/status.rs:11`）与 `platform::clock::Now`（`:12`）。
- **「唯一投影源」在值的层面今天已经成立。** 三个对外面都是控制通道 RPC 客户端，不是各自投影：CLI 的 `svc.list` 走 `serde_json::from_value::<Vec<Status>>`（`cli/svc.rs:348`）；HTTP 走 `face/api.rs:47 rpc()` → `support::rpc_result`（`face/support.rs:71`）→ `channel::request`；MCP 桥走 `face/mcp.rs:263` → `channel::request("registry.tools")`。缺的不是合并重复，而是这个值的**类型没有合法落点**。
- **「投影族」其实是三族黏在一起**（`daemon.rs` 约 350 行）：值形状构造（真正的实现在 `cli/status.rs:56/83/98`）、MCP envelope（`project_slot_response` / `_outcome`、`project_rejection`、`project_builtin_outcome`，`daemon.rs:2606-2686`）、会话状态（`catalog_version` / `catalog_cache` / `tools_page`，`daemon.rs:1337-1381`）。
- **层 rank 允许 envelope 进内核，语义不允许。** `tools/arch-map.json:5-30` 是 platform 0 < wire 1 < kernel 2 < domain 3，`rpc::Response` 属 wire（rank 1），内核依赖它不违规（1 ≤ 2）——但那等于让内核认识 JSON-RPC。
- **有一条现成的对称落点。** `problem_data`（`daemon.rs:2590-2592`）写 `{"kind": …}`，而对称的解码器 `Problem::from_error_data` 已在 `kernel/failure.rs:209-211`，被 `face/support.rs:164` 依赖。
- **§7 的三条「真实反例」全部过时**（`node tools/dep-lint.mjs` = 0 违规）：`slot` 调 `mcp::tool_error` 已不存在（`grep -rn "crate::face" src/supervise src/manage src/kernel src/platform` 为空）；`transfer`（`manage/ops/transfer.rs:17-22`）已不依赖 `status`；`install` 调 `daemon::shutdown_quiet` 只剩 `main.rs:136`，与定义同在组合根。
- **同一转换有两处实现。** `Status::live` 的 `epoch_secs`（`cli/status.rs:87-90, 109-113`）带 `ms == 0 → None` 护栏；`daemon.rs:886-893 persist_states` 自己算一遍，没有护栏。
- **「逐字节不变」这条验收今天不成立。** 没有任何测试逐字节比对 HTTP / MCP 的 JSON 响应，全是按 `Value` 键值断言；`Cargo.toml:15` 是 `serde_json = "1"` 未开 `preserve_order`，`Cargo.lock` 里没有 `indexmap`，所以线上 `Map` 是 `BTreeMap`（键按字母序），而 CLI `svc status --json` 走结构体声明序（`cli/svc.rs:711`）。

## Decision

- **投影两分。** **值投影** = 领域值 → 面中立值形状的**纯函数**，归内核；**协议投影** = 面中立值 → 某协议报文，归该对外面。判据只有一条：输出里有没有某个协议的词（`inputSchema`、`nextCursor`、JSON-RPC 帧、HTTP 状态码）。
- **第三类排除项：面级装饰。** `hasConfig` / `hasUI`（`face/api.rs:559-575`）这类字段可以在投影值之上追加，也可以读盘（`manage::plugin::web_assets`），但它不是投影，不进内核形状。
- **`kernel/projection.rs` 的内容。** `Status` / `Summary` / `Details` + `Live` + `of` / `live` / `builtin` 与私有 `epoch_secs*`。只依赖 `kernel` 与 `platform`；不出现进程、socket、文件。
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
- `Slot::set_projector` 成为「想象中的 seam」的现成反例：唯一注入点是 `daemon.rs:2693`，而读它的 `Slot::project` 与 `CallSink::Client` 都标着 `#[cfg(test)]`（`slot.rs:131/1124`），生产路径只走 `CallSink::Completion` → `projected_done`。清理票 #209 把它删掉。
- `persist_states` 的重复换算**不在本次范围**：收掉它会改变 `since_ms == 0` 时写进运行态记录的值，属行为改动，另开票带测试。
- `face/mcp_tools.rs` 与 `kernel/projection.rs` 要登记进 `tools/arch-map.json`，否则 `dep-lint` 的 V4 会红；各自由 #211 / #212 做。
- 词表新增「值投影」「协议投影」「运行面（Live）」三条（#213）。
- 未修的第三方重复（MCP 分页包在 `daemon.rs:1376-1379` 与 `face/mcp.rs:282-285` 各一份；auth 四键在 `daemon.rs:2892` 与 `face/api.rs:327` 各一份）记在 #210，`needs-triage`。
