# S9 准入：一个判断点，四种方式

## Status

Accepted。细化 [docs/architecture.md](../architecture.md) §8 的 S9 行；#171 交付。

## Context

- `src/server/auth/lan.rs`（298 行）同时装：四个准入分支（`AuthKind::{None, Bearer, Cookie}` 与 plugin token）、会话 TTL、登录失败延迟与锁定、cookie 头构造、`require_root`。
- 两个入口各自 `match lan.authenticate(...)`：`server/http/dispatch.rs` 与 `api.rs`；两处都把 `AuthKind` 直接当 `Principal`（`server/http/contract.rs`），face 再按 `AuthKind::Cookie` 做 CSRF / Origin 判断。
- 结果：安全路径没有单一测试点；新增一种准入方式要改两个入口，并把 401 / 500 投影各抄一遍。

## Decision

- **一个判断点**：`src/server/auth/admission.rs` 的 `Admission` 是 server → face 之间唯一的准入判断。接口只有两个动作：`admit(headers) -> Result<AuthKind, Rejected>`（本机 / bearer / cookie）与 `admit_plugin(token, plugin) -> bool`（plugin token）。
- **拒绝按类别**：`Rejected::{Unauthorized, Unavailable}`；调用方按类别投影成 401 / 500，文案只用于展示。不新增 `Problem` 变体——S5 的 `Problem` 是管理失败类别，不是准入结果。
- **本机免认证是一条真实 adapter**：LAN 的 `require_auth == false` 时 `admit` 返回 `AuthKind::None`；公开路由用 `Admission::local()`，而不是调用方的隐式早退。
- **`lan` 退回会话与限速**：`authenticate` 的凭证比对、`login` / `logout`、会话 TTL、plugin token 的有效期判定留在 `lan`；`plugin_token_valid` 自由函数搬进 `admission`，内部仍走同一个 `Admission`。
- **控制通道的端点 ACL（ADR-0014）不是这条准入路**，不在本 seam。

## Considered options

- **每种方式一个 trait 实现（四个实现）**：四种方式今天共享同一份 LAN 状态、同一份会话表与同一把锁，拆成四个对象只是把 `Mutex` 传四次，且要为一个共享状态维护四个实现。
- **只在 `lan` 内部收窄、保留两个调用点**：安全路径仍是两个入口，新增方式仍要改两处——没有解决真正的问题。

## Consequences

- 两个入口共用同一条判定；四个分支各有模块级测试，包含锁中毒时失败关闭。
- 新增一种准入方式只改 `Admission` 一处。
- HTTP 语义不变：401 / 303 / 403 的文案、`CSRF` 与 `Origin` 规则、登录限速都保持现状。
