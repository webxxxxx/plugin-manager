# 厂商后台免登录：`/vendor-login` 换会话

- 日期：2026-11-28
- 状态：**已决策（2026-11-28）**，实现见 `src/face/vendor_login.rs`
- 触发案例：SR6110 路由器镜像里，厂商后台（`:80`）注入的「应用插件」菜单打开的是
  svcd 控制台（`:8787`）。管理员已经登录过厂商后台，不想为控制台再输一次密码。

## 背景

设备上两套页面同主机不同端口：厂商后台在 `http://<设备>/`，svcd 控制台在
`http://<设备>:8787/`。浏览器把它们当两个源：

- 厂商的 `SID`（以及我们自己的 `svcd_session`）都是**按主机存**的 cookie，跨端口
  **会**随请求发送 —— 这一点是方案可行的基础；
- 但 `svcd_session` 只由 svcd 的登录流程签发，厂商的 `SID` 对 svcd 没有意义；
- 厂商会话**绑客户端 IP**：同一个 SID 从 PC 请求厂商 `loginStatus` 返回 SUCC，
  从设备本机请求返回 404。所以 svcd **不能**在服务端代查“这个 SID 登录了吗”，
  只有浏览器里的页面能问。

## Decision

新增 `POST /vendor-login`（以及它的 CORS 预检 `OPTIONS`）：把“浏览器里已经登录的
厂商会话”换成一对 `svcd_session` / `svcd_api` cookie，形状与 `POST /login` 完全一致
（同 TTL、同滑动续期、`POST /logout` 一样撤销）。

它是**认证入口**，和 `/login` 同级，跑在准入之前（实现在 `face/fallback.rs` 的最前面，
与 `/mcp` 同一个位置），因此必须自己证明来源：

1. **默认关闭，由宿主配置开**：只有 `svcd.conf` 里 `vendor_login = on`（默认 off）时
   这条入口才存在（否则回落到正常准入，和一个没有这个功能的 daemon 一样）。开关属于
   宿主，不属于插件：注入菜单的 `webbind` 插件（`plugins` 仓库的 sr6110 专用 `.spk`）
   不读也不写这份配置 —— 插件目录之外它只做 bind mount，不落任何文件。
2. **`Origin` 必须是设备自己的 web 根**：主机名与 `Host` 相同，端口为空 / 80 / 443，
   scheme 是 `http` 或 `https`。其它应用端口（插件页面、第三方面板）一律拒绝；
3. **必须带 `X-Vendor-OK: 1`**：自定义头让调用变成 CORS 预检，svcd 只在 `Origin`
   可信时才回 `Access-Control-Allow-Origin` / `-Credentials` / `-Headers`，别的源上的
   页面到不了处理器；
4. **浏览器侧脚本自己先问厂商**：注入的菜单脚本用浏览器 cookie 请求厂商的
   `/?_type=vueData&_tag=home_internetreg_lua_no_update_sess`，只在答 SUCC 时才发
   这条请求。

成功后响应带 `Set-Cookie` 与 CORS 头；因为 cookie 按主机存，iframe 里的
`http://<设备>:8787/` 直接就是登录态，控制台不再显示登录页。

## Considered Options

- **服务端代查厂商 SID（拒绝）**：厂商会话绑客户端 IP，从设备本机代查恒 404；
  这也是老 plugin-httpd 的 `/api/session` 需要前端 `X-Vendor-OK` 断言的原因。
- **让控制台走 vendor 反代 / 同源路径（拒绝）**：要改厂商 httpd 的路由或加一层反代，
  与“插件不自起 HTTP、宿主独占端口”的模型冲突，也把一个页面需求变成部署需求。
- **把厂商 SID 直接当 svcd 凭据（拒绝）**：svcd 无法验证它（见上），等于让任意
  `SID=` 都能登录。
- **去掉控制台密码 / 只绑 loopback（拒绝）**：LAN 上暴露无鉴权管理面；ADR-0010 明确
  认证由 `bind` 推导，不能为一条页面需求开缺口。
- **给插件 UI 的 1 小时 token 复用（拒绝）**：那是 per-plugin、按 URL 携带的凭据，
  解决的是“跨插件调用”与 URL 猜 id；控制台不是插件 UI，也不该把 token 放进可分享的 URL。

## Consequences

- **开关开着时，能访问 :8787 的人就能拿到管理员会话**：`Origin` 与 `X-Vendor-OK` 是
  浏览器里的护栏（跨源页面过不了预检），但 `curl` 之类的非浏览器客户端可以照着伪造，
  而 svcd 无法验证厂商 SID（前面说过，IP 绑定让服务端代查不可行）。所以这条能力默认
  关闭（`vendor_login = off`），只在“厂商后台就是这台设备唯一的管理入口”的镜像上由
  运维写 `vendor_login = on` 打开；想保住 svcd 密码这条线就不要开。
- 信任边界是“同一台设备的厂商 web 根”：能在 `:80` 上跑 JS 的人已经拿到后台，控制台
  与它同级。插件 UI（同一端口、路径 token）**不能**发这条请求 —— 它的 `Origin` 端口是
  `:8787`，被第 2 条拒掉。
- 免登录只覆盖“进入控制台”：会话仍是独立的一份，`POST /logout` 只清 svcd 的会话，
  不会把厂商后台踢下线；密码登录、Bearer、会话轮换都不变。
- 厂商页面用 `https` 时 iframe 会撞混合内容：设备没有 TLS，菜单脚本固定
  `http://<host>:8787/`；这条只在明文设备上有意义。
- 端点每次签发都写一条 `vendor handoff: session issued for origin …` INFO 日志，
  便于在 `svc logs svcd` 里看出免登录是否真的发生过。
- 老 plugin-httpd（`:8081`，另一套系统）自带同类的 SID 换 token；本 ADR 只覆盖 svcd，
  不把两套认证合并。
