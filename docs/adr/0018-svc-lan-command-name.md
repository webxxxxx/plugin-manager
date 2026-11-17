# `svc lan` 命令名恢复

## Status

Accepted。恢复 [ADR-0003](0003-password-auth-and-config-dir.md) 中把 `svc lan` 改名为 `svc auth` 的**命令名**；密码认证与 `bind` 语义不变。

## Context

ADR-0003 把 `svc lan` 改名为 `svc auth`，强调它已经不再是 LAN 开关，而是本地/HTTP 密码认证。实际运维里操作者仍习惯 `svc lan passwd` / `svc lan status`，而且 `lan` 是这一组凭据的领域名；`auth` 更像机制名。改名只发生在 CLI 动词上，RPC 方法与配置键一直是 `auth.status` / `auth.passwd` / `auth_passwd`，没有跟随改名。

## Decision

- CLI 的主命令名恢复为 `svc lan`：`status`、`passwd <password>`。
- `svc auth` 保留为兼容别名，现有脚本不需要改；帮助文本把 `auth` 标为 deprecated。
- RPC 方法 `auth.status` / `auth.passwd`、配置键 `auth_passwd`、密码哈希与 `bind` 推导规则都不变。
- 用户文档、生成配置注释、CLI 错误提示统一写 `svc lan`；ADR-0003 / ADR-0010 保留历史措辞，并加指针说明现行命令名。

## Consequences

- 新文档和示例使用 `svc lan`；旧脚本继续用 `svc auth`。
- 命令名变化不影响 wire 协议、配置文件或权限检查（`passwd` 仍要求 root / Administrator）。
- 这是命名回退，不是 LAN token 的回归；`svc lan enable/disable` 与 token 逻辑仍按 ADR-0010 保持删除。
