# HTTP bind 与插件 UI token

> CLI 名称后来由 [ADR-0018](0018-svc-lan-command-name.md) 恢复为 `svc lan`；本文的 `svc auth` 均指当前的 `svc lan`，`svc auth` 保留为兼容别名。

## Decision

- **`bind` 取代 `http_port` + `lan_default`**：`svcd.conf` 只写一个 `bind = <ip>:<port>`，默认 `127.0.0.1:8787`。不再保留 `lan.json`、`http_port`、`lan_default` 旧键，也不做迁移；旧配置会被当作未知键拒绝。
- **认证由 bind 推导**：loopback 不要求密码；非 loopback 必须有 `auth_passwd`。缺失密码时 daemon 降级到 `127.0.0.1:<port>` 并在日志说明，保证 `svc auth passwd` 仍可通过本地控制通道执行；设置密码后重启即可绑定 LAN。
- **删除 `svc auth enable/disable`**：没有运行时 rebind，也没有 `auth.enable` / `auth.disable` RPC。保留 `svc auth passwd` 和 `svc auth status`；`status` 显示 requested/effective bind、`authRequired`、`passwordSet`。
- **按插件签发 UI token**：`POST /api/v1/plugins/{id}/ui-token`（管理鉴权）签发一小时、绑定该 plugin id 的随机 token，返回 `/plugin-ui/{id}/{token}/`。插件页面、静态资源、`/rpc`、SSE、WS 都必须带这个路径 token；服务端校验 token 的 plugin id 与 URL 一致。
- **token 不写 cookie**：放在 URL path/`<base>` 中，相对 `fetch('rpc')`、`EventSource('events')` 和 `WebSocket` 自然继承 token；不设置 cookie，避免浏览器按 URL 自动把 B 的 cookie 带给 A。插件页面响应带 `Referrer-Policy: no-referrer`。
- **控制台“打开”先取 token**：内嵌控制台的打开动作调用 `ui-token`，再用返回的绝对/相对路径打开新窗口；不再直接打开 `/plugin-ui/{id}/`。

## Consequences

- `http_port`/`lan_default`/`lan.json` 的旧部署必须改配置；这是有意的 breaking change，不做静默迁移。
- 非 loopback 无密码时服务实际仍只监听 loopback；`auth status` 会同时显示 requested 和 effective bind。
- 插件 UI URL 带一次性会话 token；token 泄露（例如用户分享 URL）能访问该插件 UI 直到过期。token 1 小时过期，daemon 重启后全部失效。
- token 只解决“URL id 被信任”和“无 token 的跨插件调用”；它仍是同源浏览器模型，恶意同源 JS 的隔离需要独立 origin 或 sandbox iframe。
