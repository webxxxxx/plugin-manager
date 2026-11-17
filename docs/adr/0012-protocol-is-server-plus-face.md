# 协议 = 服务器 + 对外面

> 2026-11-17 修订：本 ADR 原来说的「把连接交出去」现在指把请求交给 sans-IO `StreamHandler`，**连接所有权仍归服务器**；见 [ADR-0016](0016-s4-request-handover.md)。

## Decision

- **一个协议 = 一个服务器 + 一组对外面**。服务器负责会话（监听、握手、鉴权、传输契约），对外面负责端点与投影。服务器不认识领域概念，对外面不拥有连接。
- **服务器与对外面之间只有两种结果**：内存响应，或交出 sans-IO stream handler（流式）。没有第三种，也不允许功能自己接管监听循环。
- **控制通道也是一个服务器**：daemon 在 unix socket 上监听，与 HTTP、MCP 平级；命令行是它的客户端，不是特例。
- **鉴权在服务器建立连接时完成**，沿用 ADR-0010 的 token 语义；不进入对外面。
- **新增协议只新增 `server/xxx` 与 `face/xxx_*`**，不改现有服务器，也不改领域模块。

## Consequences

- 今天 `api.rs` 同时是 HTTP 引擎、`/api/v1` 端点、WebSocket 帧机，横跨三层；迁移第 3 步把它拆到 `server/http`、`face/api`、`wire/ws`，目标降到 1500 行以下。
- 五个硬编码的 takeover 钩子（ui / export / import / install / blob）合并成统一的「交出 stream handler」交接；新增流式端点不再需要改两个引擎（unix 与 windows 各一份）。
- `blob_http.rs`、`install_http.rs`、`ui_http.rs` 这类「每个功能自带小服务器」的形态消失（`export_http.rs` 已在 #120 删除，导出变成 `face/export.rs` 的投影），协议名只在 `server/` 出现一次。
- 服务器目录里出现 `service`、`plugin`、`blob` 这类词即视为越界；对外面里出现连接、超时、背压同样视为越界。
- 反方向也成立：想在两个协议间复用端点逻辑，只能抽到对外面的公共投影，不能把服务器当工具类调用。
