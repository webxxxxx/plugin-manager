# 插件 IPC、反向代理与本地管理 API 的 Rust 最小可靠实现

调研日期：2026-11-12。范围：PluginManager 与插件进程通信、`/plugin-ui/{name}/` 反向代理、桌面端本地管理 API 三条链路。
证据分级：**实测**（本机 `rustc 1.99.1` 编译并用 `Get-Item` 读取字节数）> **一手文档**（docs.rs / crates.io / 规范原文 / 厂商文档）> **推断**（由前两者推出，未直接验证）> **未确认**。

## 结论

1. **体积不会逼你手写 HTTP。** 在同一 release 配置下（`opt-level="z"` + fat LTO + `panic=abort` + strip），x86_64-musl 静态二进制：空壳 364.7 KiB、纯 std 手写 HTTP 396.7 KiB（+32.0 KiB）、`httparse` 层 404.7 KiB（+40.0 KiB）、`tiny_http` 560.8 KiB（+196.1 KiB）、hyper 1.12 最小服务器 644.8 KiB（+280.1 KiB）、axum 0.8 748.8 KiB（+384.1 KiB）。**连 axum 都没超过 1 MiB**，≤1MB 只意味着 axum 只剩 ~270 KiB 余量给业务逻辑。[实测]
2. **嵌入式选型应由"要不要异步运行时/要不要路由"决定，而不是由体积决定。** 体积梯度是 32 KiB（无依赖）→ 40 KiB（httparse）→ 280 KiB（hyper）→ 384 KiB（axum）。要给 serde_json 留 44 KiB 左右（NDJSON JSON-RPC 实测 408.7 KiB）和插件运行时自身的逻辑。[实测]
3. **插件 IPC 首选"换行分隔 JSON（NDJSON）+ 任意可靠双向字节流"，而不是 HTTP。** MCP 2026-07-28 版 stdio 绑定就规定消息按换行分隔、**MUST NOT** 含内嵌换行，并且明确写这套线格式"在 Unix domain socket、TCP 或任何类似通道上原样可用"，自定义传输 **SHOULD** 复用它。也就是说：stdio / UDS / 命名管道共用一套分帧，不必为每种通道设计协议。[一手文档]
4. **`hyperlocal` 不必用，而且它只覆盖 Unix。** 它最后发布 0.9.1（2024-07-22），仓库最后 push 2024-07-22，依赖 hyper ^1.3 / tokio ^1.35，docs.rs 只在 unix 目标上构建。真正需要的能力是"hyper 的 `serve_connection` 能跑任意 `AsyncRead + AsyncWrite`"——我用 hyper 1.12 + tokio 1.54 在 Windows 命名管道上实测跑通 HTTP/1.1（`STATUS=200 OK`，双向收发），同一段代码形态可直接喂 `tokio::net::UnixStream`。[一手文档 + 实测]
5. **Windows 命名管道是桌面端管理 API 的正解，但默认 ACL 必须自己收紧。** `CreateNamedPipe` 传 NULL 安全描述符时，默认 DACL 给 LocalSystem/管理员/creator owner 完全控制，**并给 Everyone 和 anonymous 读权限**；tokio 只提供 `unsafe` 的 `create_with_security_attributes_raw` 让你传原始 `SECURITY_ATTRIBUTES`。`reject_remote_clients` 默认已开，`first_pipe_instance` 可防抢注。[一手文档]
6. **`AF_UNIX` 在 Windows 10 1803+ 的 OS 层存在（仅 SOCK_STREAM），但 Rust 稳定版用不上：** `std::os::windows::net` 仍是 nightly 实验 API（`windows_unix_domain_sockets`，tracking issue #150487，2026-10-05 仍 open）；tokio 的 `UnixStream` 只有 unix 实现。所以"全平台一套 UDS"在 2026-11 的稳定工具链上不成立，Windows 侧要么走命名管道，要么自己用 winsock（socket2）拼。[一手文档]
7. **反向代理的难点不是转发，是路径语义和鉴权。** axum 的 `nest` 会把匹配到的前缀**剥掉**再交给内层服务，原始 URI 要从 `OriginalUri` 取——这正是把 `/plugin-ui/{name}/` 映射成插件内部 `/` 的现成机制；但插件页面里的绝对路径（`/assets/app.js`）会逃出前缀，需要 `<base>` 注入或路径重写，**或干脆给每个插件独立 origin**。[一手文档 + 推断]
8. **loopback 上"随机端口 + token"只能算及格线，不是安全边界。** RFC 8252 §7.3 的 loopback 重定向范式（`http://127.0.0.1:{随机端口}/`，v4/v6 都试）可作为端口/token 的依据；而 W3C Local Network Access 草案明确说这类缓解"无法真正解决"本地 Web 服务的问题，路由器管理界面"必须自己实现 CSRF 防御"，且 §5.2 的 DNS rebinding 检查必须**每个新连接**都做。结论：代理层必须自己校验 `Host` + `Origin` + token，不能指望浏览器。[一手文档]
9. **WebSocket 升级仅在插件 UI 真的用 WS 时才必要。** 握手是 `Upgrade`/`Connection` + `101 Switching Protocols`（RFC 6455 §4.1）；hyper 不会自动完成升级，必须自己调 `hyper::upgrade::on`。若插件 UI 只是静态资源 + fetch，**不做 WS 可以省掉一整条隧道逻辑**。[一手文档]

## 证据

测量方法（可复现）：本机 `rustc 1.99.1 (58cf8f9c3 2026-10-13)`，host `x86_64-pc-windows-msvc`，target `x86_64-unknown-linux-musl`（rustup 无额外 linker 即可交叉链接）。全部项目用同一 profile：

```toml
[profile.release]
opt-level = "z"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = "symbols"
```

命令：`cargo build --release --target x86_64-unknown-linux-musl`，体积取产物的文件字节数。夹具放在系统临时目录，未进入本仓库。所有 handler 都是返回 `"ok"` 的最小实现，因此这些数字应视为**框架增量下界**，不是产品二进制预估。

### A. HTTP 栈体积（实测，musl 静态）

| 变体 | 依赖 | 字节 | KiB | 相对空壳 |
|---|---|---|---|---|
| 空壳（`println!("hi")`） | 无 | 373,408 | 364.7 | — |
| 纯 std 手写 HTTP/1.1 | 无 | 406,176 | 396.7 | +32,768（+32.0 KiB） |
| `httparse` 1.10 解析层 | httparse | 414,368 | 404.7 | +40,960（+40.0 KiB） |
| `tiny_http` 0.12 服务器 | tiny_http | 574,224 | 560.8 | +200,816（+196.1 KiB） |
| hyper 1.12 最小服务器 | hyper + hyper-util + http-body-util + tokio | 660,272 | 644.8 | +286,864（+280.1 KiB） |
| axum 0.8 最小服务器 | axum + tokio | 766,768 | 748.8 | +393,360（+384.1 KiB） |
| NDJSON JSON-RPC over stdio | serde_json | 418,528 | 408.7 | +45,120（+44.1 KiB） |

来源：本次实测，2026-11-12；crate 版本取自 crates.io（见下表）。

### B. 事实与出处

| 事实 | 结论 | 来源 URL | 日期/版本 |
|---|---|---|---|
| 14 个候选 crate 的当前版本与活跃度：hyper 1.12.0、axum 0.8.9、httparse 1.10.1、tokio 1.54.1、hyper-util 0.1.21、http-body-util 0.1.5、tower-http 0.7.1、interprocess 2.4.8、tiny_http 0.12.0、hyperlocal 0.9.1 | 除 `tiny_http`（2022-10-06）与 `hyperlocal`（2024-07-22）外，其余均在 2026 年有发布；`tiny_http` 已 4 年无版本 | https://crates.io/crates/hyper 、https://crates.io/crates/axum 、https://crates.io/crates/tiny_http 、https://crates.io/crates/hyperlocal 、https://crates.io/crates/interprocess | 2026-10-06 / 2026-04-14 / 2022-10-06 / 2024-07-22 / 2026-11-08 |
| hyperlocal 0.9.1 依赖 hyper ^1.3、tokio ^1.35，提供 `UnixListenerExt`/`UnixClientExt`，文档只在 unix 目标构建 | 它是 hyper 1.x 的 UDS 适配层；hyper 0.14 时代的老写法已不适用 | https://docs.rs/hyperlocal/latest/hyperlocal/ | 0.9.1（docs.rs 构建 2026-10-25） |
| hyperlocal 仓库最后 push 2024-07-22，9 个 open issue，未归档 | 维护属"可用但停滞"，不是活跃项目 | https://github.com/softprops/hyperlocal | 数据取于 2026-11-12 |
| hyper 的 upgrade 模块：HTTP/1.1 upgrade 的其他前置条件"由你负责"，需自己调 `hyper::upgrade::on` 拿升级后的连接 | hyper 不代做 WebSocket 握手；反向代理要自己转发 `101` 并桥接两条字节流 | https://docs.rs/hyper/latest/hyper/upgrade/index.html | hyper 1.12.0 |
| WebSocket 客户端握手：连接初始为 CONNECTING，客户端发送 opening handshake；服务端以 `101 Switching Protocols` 应答，非 101 即握手失败 | 代理必须原样中继 `Upgrade`/`Connection` 头，并在 101 后转为裸字节转发 | https://www.rfc-editor.org/rfc/rfc6455.txt | RFC 6455，2011-12 |
| axum `Router::nest`：嵌套路由"看不到原始请求 URI，匹配到的前缀已被剥离"，需要原始 URI 时用 `OriginalUri`；通配路由则保留完整 URI | 前缀改写有现成机制（nest + OriginalUri），不必自己切字符串 | https://docs.rs/axum/latest/axum/struct.Router.html | axum 0.8.9 |
| MCP stdio 绑定：消息按换行分隔且 MUST NOT 含内嵌换行；服务器 MAY 向 stderr 写日志，客户端 SHOULD NOT 把 stderr 当错误信号；stdout MUST 只出现合法 MCP 消息 | 日志必须走 stderr、stdout 只放协议帧；这是硬约束 | https://modelcontextprotocol.io/specification/latest/basic/transports/stdio | 最新版（页内列出 2026-07-28 为最新规范版本） |
| 同一页：该线格式（每条换行分隔的 JSON-RPC，跑在可靠双向字节流上）"在 Unix domain socket、TCP 或任何类似通道上原样可用"，自定义传输 SHOULD 复用它 | 一套 NDJSON 分帧可同时服务 stdio / UDS / 命名管道，无需为管道另设协议 | https://modelcontextprotocol.io/specification/latest/basic/transports/stdio | 同上 |
| MCP 2025-06-18 版 transports：stdio 段同样规定换行分隔、禁止内嵌换行、stderr 用于日志、stdout 不得写非 MCP 内容 | 该约定至少自 2025-06-18 起稳定，可安全依赖 | https://modelcontextprotocol.io/specification/2025-06-18/basic/transports | 2025-06-18 |
| JSON-RPC 2.0 规范（2013-01-04）：请求以 `id` 关联响应；Notification 无响应 | 并发请求靠 `id` 复用，但规范本身不提供分帧与顺序保证 | https://www.jsonrpc.org/specification | 2.0，2013-01-04 |
| tokio `ServerOptions`：`pipe_mode` 默认 `PipeMode::Byte`（可选 Messages）；`reject_remote_clients` 默认拒绝远程客户端；`first_pipe_instance` 防第二实例；`max_instances` 1–254；ACL 只能通过 `unsafe fn create_with_security_attributes_raw(attrs: *mut c_void)` 传原始 `SECURITY_ATTRIBUTES` | 字节模式适合 HTTP/NDJSON；安全描述符是 unsafe 裸指针级别，没有安全封装 | https://docs.rs/tokio/latest/tokio/net/windows/named_pipe/struct.ServerOptions.html | tokio 1.54.1 |
| Windows 命名管道类型模式：`PIPE_TYPE_BYTE`（默认）不区分写入边界；`PIPE_TYPE_MESSAGE` 把每次 write 当一个消息单元，且总按 write-through 语义写 | 消息模式能省掉应用层分帧，但跨平台行为与字节流不一致，HTTP 只能用字节模式 | https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-type-read-and-wait-modes | 厂商文档，2026-11-12 取 |
| Windows 命名管道安全：`CreateNamedPipe` 传 NULL 时使用默认安全描述符，其 ACL 给 LocalSystem、管理员、creator owner 完全控制，并给 Everyone 与 anonymous **读**访问；可用 `SetSecurityInfo` 改 DACL | 默认 DACL 对"只发启停指令"的管理口勉强够用（写未授予 Everyone），但会泄露读；应显式设 DACL | https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights | 厂商文档，2026-11-12 取 |
| `AF_UNIX` 自 Windows Insider Build 17063 起支持，仅 `SOCK_STREAM`，`SOCK_DGRAM` 未实现 | OS 层可用；但见下一行的语言侧限制 | https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/ | 2017-12-19 |
| `std::os::windows::net`（`UnixStream`/`UnixListener`）是 nightly 实验 API，feature gate `windows_unix_domain_sockets`，tracking issue 仍 open | 稳定版 Rust 不能用 std 在 Windows 上开 UDS；Windows 侧需命名管道或自建 winsock | https://doc.rust-lang.org/std/os/windows/net/index.html 、https://github.com/rust-lang/rust/issues/150487 | issue #150487，更新于 2026-10-05 |
| interprocess 2.4.8：local socket 是旗舰抽象，Windows 上由命名管道实现，Unix 上复用 std 的 UDS；"从不插入自己的分帧或元数据"，写入的字节原样到达；异步只支持 tokio；维护状态自标 passive | 跨平台统一 IPC 的安全封装选项，代价是引入一层抽象与被动维护节奏 | https://docs.rs/interprocess/latest/interprocess/ | 2.4.8（2026-11-08） |
| RFC 8252 §7.3：桌面应用可在 loopback 上监听，URI 用 loopback IP 字面量 + 系统分配的临时端口（示例 `http://127.0.0.1:51004/...`），建议 v4/v6 都尝试 | "随机端口 + 127.0.0.1"是有规范依据的本地回调范式 | https://www.rfc-editor.org/rfc/rfc8252.txt | RFC 8252，2017-10 |
| W3C Local Network Access 草案 §5.2：缓解基于 UA 实际连接的 IP，且**每个新连接**都必须检查，否则 DNS rebinding 可绕过；§5.3：该提案"只缓解、无法解决"本地 Web 服务问题，"路由器的 Web 管理界面必须自己防 CSRF，不应依赖按本规范实现的 UA"；§1.1 点名"在用户 loopback 地址上跑 Web 界面的软件"常常假设了并不存在的保护 | loopback 服务必须自己校验 Host/Origin/token；浏览器侧新机制也不替你兜底 | https://wicg.github.io/local-network-access/ | Draft CG Report，2026-08-07 |
| hyper 的 HTTP/1.1 服务端由 `server` + `http1` feature 开启（本次构建即用这两个 feature + `hyper-util/tokio` + `http-body-util` 跑通） | feature 粒度可裁剪，用不到 client/h2 就不编进来 | https://docs.rs/crate/hyper/latest/features | hyper 1.12.0 |

### C. 第一手功能验证（实测，2026-11-12）

| 验证项 | 结果 | 说明 |
|---|---|---|
| hyper 1.12 + tokio 1.54 在 Windows 命名管道 `\\.\pipe\plugin-manager-ipc-probe` 上跑 HTTP/1.1 | `STATUS=200 OK`，`BODY=ok-over-pipe` | 服务端 `ServerOptions::create` + `connect()` + `http1::Builder::serve_connection(TokioIo::new(pipe))`；客户端 `hyper::client::conn::http1::handshake(TokioIo::new(NamedPipeClient))`。**双向都通**，证明"HTTP over 命名管道"不需要 hyperlocal 或自写解析 |
| 同一形态用于 UDS | 未直接跑（本机为 Windows 主机） | 依据：`serve_connection` 只要求 `hyper::rt::Read + Write`，`TokioIo` 为任意 `AsyncRead + AsyncWrite` 提供实现（hyper-util）。**推断**，风险低 |

## 对技术选型的影响

确定性标注：**高**＝有实测或规范硬约束支撑；**中**＝一手文档直接支持但未在本项目验证；**低**＝推断。

### 嵌入式（musl 静态，≤1MB）

| 链路 | 建议 | 体积量级 | 确定性 |
|---|---|---|---|
| 插件 IPC | **NDJSON（JSON-RPC 2.0）+ stdio**，日志一律走 stderr；不引 HTTP 框架。若插件需要带外通道，Unix 上用 `std::os::unix::net::UnixListener` 跑**同一套 NDJSON 分帧**（MCP 明确说该线格式跨通道复用） | 空壳 364.7 KiB + serde_json 44.1 KiB ≈ **409 KiB**，业务逻辑还有 ~600 KiB 余量 | 高（体积实测；分帧规范一手） |
| 插件 UI 反代 | **不做**。嵌入式不承担 Web UI 托管，交给桌面端 | 0 | 高 |
| 本地管理 API | **不做**（无桌面外壳时无意义）；若未来需要，复用 NDJSON 分帧而非新协议 | 0（复用分帧） | 中 |

最小依赖清单（嵌入式）：`serde` + `serde_json`（NDJSON）+ 可选 `httparse`（仅当必须给插件暴露 HTTP 语义，+40 KiB）。**不要**在嵌入式引入 axum（+384 KiB，余量只剩 ~270 KiB）。
风险：`tiny_http` 虽比 hyper 轻（+196 KiB vs +280 KiB），但 2022-10 后无版本（见证据表），阻塞式模型也不便与异步插件运行时共存 → 不推荐。

### 桌面（可加依赖）

| 链路 | 建议 | 体积量级 | 确定性 |
|---|---|---|---|
| 插件 IPC（带外/长连接） | 一套 NDJSON 分帧跑在**平台原生通道**上：Unix `tokio::net::UnixListener`，Windows `tokio::net::windows::named_pipe`（字节模式）。两端都包成 `AsyncRead + AsyncWrite`，分帧代码只写一遍 | 桌面不敏感（数十 MiB 级） | 高（命名管道实测跑通；UDS 同形态为推断） |
| HTTP 需求（插件若提供 HTTP 语义） | 直接用 hyper 1.x `serve_connection` 喂上述流；**不必引 hyperlocal**（unix-only + 2024-07 后停滞） | +280 KiB（若与桌面共用同一二进制） | 高（实测） |
| 反向代理 `/plugin-ui/{name}/` | hyper 1 + hyper-util + tokio + http-body-util；路由用 axum `nest("/plugin-ui/{name}")` + `OriginalUri` 还原原始路径。**前缀改写**：转发时去掉前缀；**绝对路径逃逸**：注入 `<base href="/plugin-ui/{name}/">` 或做响应体路径重写，**更稳的是每插件独立 origin** | axum 全套 +384 KiB | 高（nest/OriginalUri 一手）；路径重写与 base 注入为**中/低** |
| SPA fallback | 仅对 `Accept: text/html` 的 404 回落到 `index.html`；不要对所有 404 回落（会把缺失的 JS 变成 HTML，掩盖错误） | 0 | 低（工程判断，无规范） |
| WebSocket | **按需**。插件 UI 若只用 fetch/静态资源，不做 WS，省掉 101 中继与双向字节桥接；若要做，注意 hyper 不自动升级，须 `hyper::upgrade::on` | 0 / 一条隧道逻辑 | 高（hyper 文档 + RFC 6455） |
| 本地管理 API | Windows **命名管道** + Unix **UDS**（不要 TCP loopback）：`first_pipe_instance(true)` 防抢注、`reject_remote_clients` 保持默认、ACL 用 `create_with_security_attributes_raw` 显式收紧（或改用 interprocess 换安全封装） | 0 额外（tokio 已有） | 高（tokio 文档 + Windows 安全文档） |
| loopback 代理的鉴权 | 绑定 `127.0.0.1`（必要时 `[::1]`）+ **系统分配端口**（RFC 8252 §7.3）+ 每会话随机 token（放 `Authorization` 或专用 header，别放 URL query 以免进日志/Referer）；**校验 `Host` 白名单**（防 DNS rebinding）与 `Origin`（防 CSRF）；LNA §5.3 明确要求本地服务自己兜底 | 0 | 高 |

桌面完整依赖清单：`tokio`（rt-multi-thread, net, io-util, macros）、`hyper`（server, http1, client）、`hyper-util`（tokio）、`http-body-util`、`axum`（路由/提取器）、`tower-http`（可选：trace/compression）、`serde`/`serde_json`、`interprocess`（可选，仅当不想碰 unsafe 的 SECURITY_ATTRIBUTES）。**不引**：hyperlocal（unix-only、停滞）、tiny_http（停滞、阻塞式）。

### 明确排除

- **"全平台统一 UDS"**：Windows 的 `AF_UNIX` 在 OS 层有，但 Rust 稳定版没有 API（`std::os::windows::net` 仍 nightly，tokio 无实现）→ 2026-11 只能"Unix 用 UDS、Windows 用命名管道"，或自建 winsock。**确定性：高**。
- **用 TCP loopback 做管理 API**：可被同机任意进程连接，且默认无 `Host`/`Origin` 校验就会被浏览器侧请求打穿（LNA §5.3）。**确定性：高**（规范 + 安全推理）。

## 未确认项

1. **interprocess 的 ACL / 消息模式能力边界**：docs.rs 的 `os::windows::security_descriptor`、`ListenerOptionsExt` 页面抓取超时，未能确认它是否提供安全的 `SecurityDescriptor` 封装、是否支持 `PipeMode::Messages`、Windows 上 `GenericNamespaced` 与 `NamedPath` 两种命名方式的差异。**需补查**：`docs.rs/interprocess/latest/interprocess/local_socket/` 与 `os::windows` 模块。
2. **interprocess 的实际维护力度**：crate 徽章自标 `maintenance-passive`，2026-11-08 仍有发布，但仓库活跃度（issue 响应、CI 覆盖率）未量化。
3. **`tiny_http` 的线程模型与 HTTP/1.1 能力细节**（keep-alive、chunked、是否每连接一线程）：未抓取其文档正文，仅由"2022-10 后无版本"推断其不适合新项目。
4. **Windows / macOS 目标下的实际二进制体积**：本次只测了 `x86_64-unknown-linux-musl`；桌面构建的体积未测（按逻辑不构成约束）。
5. **反向代理 crate 生态**（如 `hyper-reverse-proxy`）的维护状态与是否够用：未查。
6. **`<base>` 注入 / 响应体路径重写的实测效果**：未验证（不同前端构建工具对 `base` 的支持差异大，`vite` 的 `base` 配置与运行时 `document.baseURI` 行为需单独验证）。
7. **打包后的 musl 静态二进制在目标环境的真实表现**（DNS、证书、`/proc` 依赖等）：未测，与本问题正交。
8. **MCP "latest" 的确切版本号**：页面正文列出 2026-07-28 为最新规范版本，但未抓到显式 "Version 2026-07-28" 标签；引用该页时应以页面 URL 为准，避免写死版本号。
