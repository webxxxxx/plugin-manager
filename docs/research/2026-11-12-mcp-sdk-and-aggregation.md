# MCP 协议侧可行实现：LiteMCP 辨析、聚合型 Server 的最小协议面、以及 ≤1MB 内嵌的体积/维护权衡

- 调研日期：2026-11-12（Asia/Shanghai）
- 调研范围：MCP 规范当前版本与传输、生命周期/版本协商、JSON-RPC 结构与错误码、规范对"聚合/代理"的态度、LiteMCP 实体辨析、Rust/C/C++/Zig 侧 SDK 与实现
- 方法：只采信一手来源（modelcontextprotocol.io 规范站、GitHub 仓库 README 原文、crates.io / npm / PyPI 注册表 API、docs.rs）。凡未实测者标注【推断】；凡查不到者进"未确认项"。
- 标注约定：**规范条文**＝规范原文；**注册表实测**＝我直接查询 crates.io/npm/PyPI/docs.rs 接口得到的元数据；**【推断】**＝我基于前两者的判断，未经构建或压测验证。

---

## 结论

1. **先纠正一个前提：MCP 当前版本是 `2026-07-28`，不是 2025-06-18，而且是一次断裂式改版。** 该版本删除了 `initialize`/`notifications/initialized` 握手、删除了协议级 session（含 `Mcp-Session-Id`）、删除了服务端发起 JSON-RPC 请求的能力，改为**每个请求自带 `_meta` 版本与能力**，并把 `server/discover` 定为**服务端 MUST 实现**。对"内嵌一个最小 Server"来说这是**利好**：握手状态机、会话管理、服务端反向往来全部不用写。
2. **但兼容面反而变宽了：必须考虑 dual-era（现代 + 旧版握手）**。官方 TypeScript SDK 的 v2 于 **2026-07-27** 才发布并对齐 `2026-07-28`，且 v1.x 明确承诺在 v2 发布后**至少六个月**继续收 bug/安全修复。也就是说 2026-11 的存量客户端仍有大量 v1 代（2025-11-25 及更早、走 `initialize` 握手）在跑。只做现代版会把它们全部挡在门外。
3. **"LiteMCP"不是单一项目，至少有三个同名实体，且没有一个能直接解决本项目的体积问题**：(a) `wong2/litemcp`——TypeScript，MIT，README 已写明"**will no longer be maintained**"，最后一版 0.9.0 发布于 **2025-01-23**；(b) crates.io `litemcp`（DeamonDev888）——Rust，MIT/Apache-2.0，"零 async runtime、只用 serde"，但**只做 Streamable HTTP，不做 stdio**，2026-10-22 才创建、不足一个月 20 个版本、总下载 407；(c) PyPI `litemcp`——Python，**客户端侧**封装（MCPServerManager + LangChain/Agent SDK 适配），与服务端实现无关。名字相同不代表同一血统，引用时必须带注册表和作者。
4. **规范没有"聚合器/代理"这个角色，但 `2026-07-28` 首次把聚合场景写进了条文**：工具名的唯一性只在单 Server 内成立；"从多个 server 聚合工具的客户端或代理会遇到重名，SHOULD 采用前缀消歧"，并明确 `serverInfo.name` **不保证唯一、不可用于消歧**。这是规范层面对"一个 Server 转发到多个后端"的**承认**（不是禁止，也不是专门定义）。
5. **聚合器最容易踩的一条：`tools/list` 的结果在一次客户端生命周期内不得随连接变化。** `2026-07-28` 要求 `tools/list` 返回确定顺序，且工具集合 MUST NOT 因连接或其它请求的副作用而变；想让客户端感知"插件装上/卸下"，必须让服务端声明 `tools:{listChanged:true}` **并实现 `subscriptions/listen`** 长请求流。若 PluginManager 支持运行时装卸插件而我们不实现订阅，就必须声明 `listChanged:false`，此时列表变更只能靠 `ttlMs` 过期或客户端重连才能被看到——这是产品行为，不是实现细节。
6. **Rust 现成 SDK 与 ≤1MB 目标正面冲突，不是"略超"而是量级不符**：官方 `rmcp`（Tier 1，3.9.0 / 2026-11-05）源码 45,880 行、crate 包 677 KB、硬依赖 `tokio`+`futures`+`serde_json`+`tracing`+`tokio-util`，MSRV 1.88、edition 2024；`rust-mcp-sdk`（2.0.0 / 2026-08-27）11,973 行；`mcp-sdk-rs` 自述 **WIP、not ready for production**，最后一版停在 **2025-11-13**（早于 2026-07-28 改版），传输只有 WebSocket + stdio。
7. **C/C++/Zig 没有官方 SDK**：官方 10 个 SDK 清单（TS/Python/C#/Go/Rust/Ruby/Java/Swift/PHP/Kotlin）里没有 C、C++、Zig。第三方 C++ 有 `cxxmcp`（C++17，自称 conformance 272/272，且"**no runtime dependencies beyond the standard library**"）与 `cpp-mcp`（仅实现 2024-11-05）；Zig 侧**未找到 SDK**。
8. **结论性判断**：在 ≤1MB musl 静态 + 内嵌 + 只做 stdio `tools/list`+`tools/call` 这组约束下，**自研最小 MCP 层是唯一自洽路线**，而不是"用 SDK 再裁剪"。原因是 SDK 的体积不是来自协议复杂度，而是来自 async 运行时与 HTTP/TLS/OAuth 栈——而这些恰好是 stdio 内嵌场景全都不需要的部分。反过来，自研的风险集中在**协议细节的正确性**（`_meta` 必填字段、`resultType`、`ttlMs`/`cacheScope`、era 判定），不在算法复杂度。

---

## 证据

### A. 规范版本、传输、生命周期

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 规范版本号形如 `YYYY-MM-DD`，表示最后一次不兼容变更的日期；当前 current 版本是 `2026-07-28` | 2026-11 的"当前规范"不是 2025-06-18；引用规范必须写明修订号 | https://modelcontextprotocol.io/docs/2026-07-28/learn/versioning | 2026-07-28 |
| "There is no negotiation handshake. Every request carries its protocol version, and the server accepts or rejects each request independently"；版本不匹配返回 `-32022 UnsupportedProtocolVersionError` 并列出 `supported` | 现代版没有握手；版本协商退化为"每请求声明 + 错误回退" | https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning | 2026-07-28 |
| 术语定义：**Modern** = 2026-07-28 及以后（每请求元数据）；**Legacy** = 2025-11-25 及以前（`initialize` 握手）；**Dual-era** = 两者都支持。并给出 6×6 兼容矩阵 | dual-era 是规范明确承认的实现形态 | https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning | 2026-07-28 |
| stdio 上探测 era：先发 `server/discover`；返回 `DiscoverResult` → 现代；返回**可识别的现代错误**（如 `UnsupportedProtocolVersionError`）→ 现代但不支持该版本；返回其它错误或超时 → legacy，回退 `initialize` | 探测算法可直接实现为一个三态判定；回退**不得**绑定某个具体错误码 | https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio | 2026-07-28 |
| dual-era 服务端按"客户端怎么开场"选语义：带现代 `_meta` 的请求按现代无状态处理；`initialize` 请求选中 legacy 语义（stdio 下作用于进程，HTTP 下作用于 session）；可以同端点/同进程并发服务两种 era | 内嵌聚合 Server 用一个分支就能同时服务两代客户端 | https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning | 2026-07-28 |
| 传输只有两种：stdio（换行分隔 JSON-RPC、MUST NOT 含内嵌换行、stdout 只允许合法 MCP 消息、日志走 stderr）与 Streamable HTTP（单端点 POST + 可选 per-request SSE） | 内嵌 stdio 场景只需实现换行分帧；stderr 是唯一合法日志通道 | https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio | 2026-07-28 |
| Streamable HTTP 绑定页原文："Streamable HTTP was introduced in protocol version **2025-03-26** as a replacement for the HTTP+SSE transport from protocol version **2024-11-05**"；该页同时列明 2026-07-28 的三处破坏性变化（移除 GET 流、移除 session） | 时间线确认到"引入版本"级别：2024-11-05 HTTP+SSE → 2025-03-26 Streamable HTTP → 2026-07-28 再改 | https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http | 2026-07-28 |
| Streamable HTTP 每请求必需头：`MCP-Protocol-Version`（值 MUST 与 body `_meta` 一致，否则 `400` + `-32020 HeaderMismatch`）、`Mcp-Method`（所有请求）、`Mcp-Name`（`tools/call`/`resources/read`/`prompts/get`） | 将来若做 HTTP，这三个头的生成+校验是合规必做项；stdio 下完全不存在 | https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http | 2026-07-28 |
| `2026-07-28` 主要变更：移除协议级 session 与 `Mcp-Session-Id`；移除握手；新增 `server/discover`（MUST）；HTTP GET 端点与 `resources/subscribe` 被 `subscriptions/listen` 取代；移除 `ping`、`logging/setLevel`、`notifications/roots/list_changed`；MRTR 取代服务端发起请求；所有 result 必带 `resultType`；移除 SSE 可恢复性与重投递 | 现代版把这些能力**删掉**了，最小 Server 的可选面因此变小；但 legacy 客户端仍会发 `ping` | https://modelcontextprotocol.io/specification/2026-07-28/changelog | 2026-07-28 |
| `server/discover` 返回 `supportedVersions`、`capabilities`、`_meta['io.modelcontextprotocol/serverInfo']`、可选 `instructions`，并支持缓存（`ttlMs`/`cacheScope`） | 这是现代版唯一的能力宣告入口，必须实现 | https://modelcontextprotocol.io/specification/2026-07-28/server/discover | 2026-07-28 |
| 服务端 **MUST NOT** 发起 JSON-RPC 请求；客户端不发响应。服务端只能回 result/error，外加与请求作用域的 notification（`notifications/progress`、`notifications/message`） | 内嵌 Server 不需要任何"反向调用客户端"的状态机 | https://modelcontextprotocol.io/specification/2026-07-28/basic/patterns | 2026-07-28 |

### B. JSON-RPC 结构与错误码

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 全部消息 MUST 遵循 JSON-RPC 2.0；请求 id MUST 为 string 或 integer 且 **MUST NOT 为 null**；notification MUST NOT 带 id | 分帧后需要一个非空 id 校验 | https://modelcontextprotocol.io/specification/2026-07-28/basic | 2026-07-28 |
| 每个 result MUST 带 `resultType`：`"complete"` 或 `"input_required"`；客户端对缺省值 MUST 视作 `"complete"` | 我们只需产出 `complete`；同时因"缺省视为 complete"，老客户端也不会被我们省略该字段弄坏 | https://modelcontextprotocol.io/specification/2026-07-28/basic | 2026-07-28 |
| 错误码分区：标准 JSON-RPC `-32700/-32600..-32603`；`-32020..-32099` 保留给规范（`-32020 HeaderMismatch`、`-32021 MissingRequiredClientCapability`、`-32022 UnsupportedProtocolVersion`）；`-32000..-32019` 为历史遗留、新实现 SHOULD NOT 使用；`-32002` 资源未找到在 2026-07-28 改为 `-32602` | 最小实现要处理：解析失败、非法请求、未知方法、参数非法、内部错误 + 三个新码中的 `-32022`/`-32021` | https://modelcontextprotocol.io/specification/2026-07-28/basic | 2026-07-28 |
| 每个请求的 `_meta` 里 `io.modelcontextprotocol/protocolVersion`（必需）与 `io.modelcontextprotocol/clientCapabilities`（必需）必须存在；缺任一 → MUST 返回 `-32602` | 这是"最少但必须"的校验，漏掉会被判定不合规 | https://modelcontextprotocol.io/specification/2026-07-28/basic | 2026-07-28 |
| `tools/list`、`prompts/list`、`resources/list`、`resources/templates/list`、`resources/read`、`server/discover` 的结果 **MUST** 带缓存提示 `ttlMs`（>=0）与 `cacheScope`（`public`/`private`） | 即使是"最小"实现，`tools/list` 也**必须**多写两个字段，否则不合规 | https://modelcontextprotocol.io/specification/2026-07-28/server/utilities/caching | 2026-07-28 |

### C. 工具（Tools）与聚合

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 支持工具的 Server **MUST** 声明 `capabilities.tools`；`listChanged` 表示是否会发列表变更通知 | 聚合器至少要宣告 `tools` | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | 2026-07-28 |
| 工具集合"MAY be empty and MAY change over time, but **MUST NOT** vary per-connection or as a side effect of other requests on the connection"；可按请求的授权范围变化 | 插件热插拔若要立即可见，只能走显式通知，不能靠"下次 list 就变" | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | 2026-07-28 |
| Server **SHOULD** 以确定性顺序返回工具（利于客户端缓存与 prompt cache 命中） | 聚合器应按稳定键（如插件 id + 工具名）排序 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | 2026-07-28 |
| 工具名 SHOULD 1–128 字符、区分大小写、仅 `A-Za-z0-9_-` 与 `.`；**唯一性只在单个 server 内成立**。"Clients or proxies that aggregate tools from multiple servers MAY encounter naming collisions ... SHOULD implement a disambiguation strategy such as prefixing tool names with a server identifier"；`serverInfo.name` 不保证唯一、SHOULD NOT 用于消歧 | **规范明确承认聚合/代理场景**，并把消歧责任放在聚合方：建议 `<pluginId>.<toolName>` 前缀 | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | 2026-07-28 |
| 同一"Tool Names / 聚合消歧"段落在 `2025-11-25` 版本中**不存在**（该版只写到"unique within a server"） | 聚合指引是 2026-07-28 新增，属于新近条文 | https://modelcontextprotocol.io/specification/2025-11-25/server/tools | 2025-11-25 |
| `notifications/tools/list_changed` 在 2026-07-28 只发给**已开 `subscriptions/listen` 且 `toolsListChanged:true`** 的客户端；架构文档给的完整流程是 listen → `notifications/subscriptions/acknowledged` → 变更通知（带 `_meta.subscriptionId`）→ 客户端重新 `tools/list` | 想发列表变更 = 必须支持长请求流；否则只能 `listChanged:false` | https://modelcontextprotocol.io/specification/2026-07-28/server/tools ・ https://modelcontextprotocol.io/docs/2026-07-28/learn/architecture | 2026-07-28 |
| 工具错误分两类：协议错误走 JSON-RPC `error`（如未知工具用 `-32602`），工具执行错误走 `result.isError:true` + `content` | 插件崩溃/超时应映射成 `isError:true`（模型可自纠），而不是 `-32603` | https://modelcontextprotocol.io/specification/2026-07-28/server/tools | 2026-07-28 |
| 规范未定义"aggregator/gateway/proxy"这一角色，架构上只有 Host/Client/Server；一个 Server 就是一个 Server | "聚合"不是协议特性，而是"一个 Server 实现内部怎么实现"的自由；无需特殊扩展或协商 | https://modelcontextprotocol.io/docs/2026-07-28/learn/architecture | 2026-07-28 |

### D. 生态时间线与"客户端换代"证据

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 官方 TypeScript SDK v2（`@modelcontextprotocol/server` / `@modelcontextprotocol/client`）实现 `2026-07-28`，v2 发布于 **2026-07-27**；v1.x（`@modelcontextprotocol/sdk`）在其后**至少六个月**继续收 bug/安全更新 | 到 2027-01 前后，v1 代客户端仍在支持窗口内 → dual-era 是必要项 | https://github.com/modelcontextprotocol/typescript-sdk （README main 分支） | v2 发布 2026-07-27 |
| `@modelcontextprotocol/sdk` 最新版 1.32.2，unpacked 4,492,221 B、701 个文件，依赖含 ajv/express/hono/jose | 参照系：官方 TS 实现的体量与 Node 生态绑定，与内嵌无关，但说明"官方实现"从不以体积为目标 | https://registry.npmjs.org/@modelcontextprotocol/sdk/latest | 1.32.2（注册表查询 2026-11-12） |
| 官方 SDK 清单共 10 个：TS/Python/C#/Go/Rust/Ruby（Tier 1）、Java（Tier 2）、Swift/PHP/Kotlin（Tier 3）；"SDKs adopt new protocol revisions on their own schedules" | **没有 C / C++ / Zig 官方 SDK**；且各 SDK 对齐修订版的节奏不一致 | https://modelcontextprotocol.io/docs/2026-07-28/sdk | 2026-07-28 |

### E. Rust SDK 实测（crates.io / docs.rs 查询）

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `rmcp`（官方，Apache-2.0）：最新 3.9.0，crate 包 677,294 B，源码 45,880 行、69 个文件，72 个版本，总下载 47.8M，MSRV **1.88**、edition 2024 | 官方 SDK 体量 = 本项目目标二进制体积的数倍（源码层面即是） | https://crates.io/api/v1/crates/rmcp | 3.9.0 / 2026-11-05 |
| `rmcp` 传输特性：`transport-io`（服务端 stdio）、`transport-streamable-http-server` 等；默认 feature 为 `base64, macros, server`；硬依赖 `futures`、`indexmap`、`serde`、`serde_json`、`sha2`、`thiserror`、`tokio`、`tokio-util`、`tracing` | 想用 rmcp 就必须接受 tokio 异步运行时 + tracing 等固定成本 | https://docs.rs/crate/rmcp/3.9.0 ・ https://crates.io/api/v1/crates/rmcp | 3.9.0 / 2026-11-05 |
| `rmcp` README 自述为官方 Rust SDK，指向 `2026-07-28` 规范；传输矩阵：客户端 `TokioChildProcess` / `StreamableHttpClientTransport`，服务端 `stdio` / `StreamableHttpService` | 服务端 stdio + tools/list + tools/call 是官方一等场景，能力不缺 | https://docs.rs/crate/rmcp/3.9.0 | 3.9.0 |
| `rust-mcp-sdk`（MIT，rust-mcp-stack）：2.0.0，167,831 B，11,973 行，MSRV 1.80，feature 含 `stdio`/`sse`/`streamable-http`/`server`/`client`/`auth`/`macros`，默认全开 | 第二个"完整"选项，体量约 rmcp 的 1/4，但仍是 tokio + (可选) axum/reqwest 路线 | https://crates.io/api/v1/crates/rust-mcp-sdk | 2.0.0 / 2026-08-27 |
| `mcp-sdk-rs`（MIT，jgmartin）：0.3.4，32,151 B，1,484 行，edition 2021，最后发布 **2025-11-13**，总下载 13,139；README 顶部警告 "This SDK is currently a work in progress and is not ready for production use"，传输只有 WebSocket 与 stdio，fork 自 `Derek-X-Wang/mcp-rust-sdk` | 体量最小，但已停止演进、自我否认可用于生产、且早于 2026-07-28 改版 | https://crates.io/api/v1/crates/mcp-sdk-rs ・ https://github.com/jgmartin/mcp-sdk-rs | 0.3.4 / 2025-11-13 |

### F. "LiteMCP"辨析（三个不同实体）

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| npm `litemcp`（作者 wong2）：TypeScript，"A TypeScript library that simplifies MCP server development"，MIT，最后一版 0.9.0，unpacked 33,579 B；依赖 `@modelcontextprotocol/sdk`、`zod-to-json-schema`、`execa`、`mcp-proxy` | 是**官方 TS SDK 之上的糖**，不解决体积问题（Node 运行时即已出局） | https://registry.npmjs.org/litemcp | 0.9.0 / 2025-01-23（注册表 `time.modified` 同日，此后无发布） |
| 该仓库 README 顶部写明 "**Deprecate notice** ... the official SDK already offers a high-level API similar to this project, this project **will no longer be maintained**" | **已明确停止维护**，不是"维护缓慢" | https://github.com/wong2/litemcp （README 原文） | 截至 2026-11-12 |
| crates.io `litemcp`（作者 DeamonDev888）：Rust，"litemcp ... 最轻的 Rust MCP 库：Streamable HTTP stateless 专用（POST/SSE on a bound socket），**零 async runtime，仅 serde**"，MIT OR Apache-2.0，0.10.3，crate 374,479 B，11,003 行 | 唯一"以体积为卖点"的 Rust 实现，但**只有 Streamable HTTP、没有 stdio**——与本项目的 stdio 内嵌方向正好相反 | https://crates.io/api/v1/crates/litemcp | 0.10.3 / 2026-11-11；crate 创建于 2026-10-22，总下载 407 |
| PyPI `litemcp`（作者 myan / yanmxa）：Python，MIT，"A minimal, lightweight **client** designed to simplify SDK adoption"，提供 `MCPServerManager`、`agent_sdk_tools()`、`langchain_tools()`，依赖 `mcp`、`openai-agents`、`langchain-core` | 这是**客户端侧**胶水，与服务端实现无关 | https://pypi.org/pypi/litemcp/json | 0.1.1 / 2025-03-21 |

### G. 现成"代理/聚合"工具的实际形态（对照参考）

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| `mcp-proxy`（Python，sparfenyuk）：在 stdio ↔ SSE/Streamable HTTP 之间转换；支持 `--named-server` 多后端，但每个后端暴露在**独立路由** `/servers/<name>/` 下，而非合并成一份工具清单 | 现存主流"proxy"做的是**传输代理**，"把多个后端的工具合并成一个 tools/list"仍是空白/自研区（也解释了为什么规范只在命名冲突处给指引） | https://pypi.org/pypi/mcp-proxy/json | 0.15.0 / 2026-11-09 |
| `litemcp`（npm）自身也把 `mcp-proxy` 作为依赖，用于把 stdio server 暴露成 SSE | 生态里的"代理"默认指传输层 | https://registry.npmjs.org/litemcp | 0.9.0 |

### H. C / C++ / Zig 侧

| 事实 | 结论 | 来源 URL | 日期或版本 |
| --- | --- | --- | --- |
| 官方 SDK 清单无 C/C++/Zig | 这三个语言**没有**官方实现 | https://modelcontextprotocol.io/docs/2026-07-28/sdk | 2026-07-28 |
| `cxxmcp`（C++17，MIT）：自称 "production-ready C++17 SDK"、覆盖 `2024-11-05` → `2026-07-28`、支持 stdio / process-stdio / Streamable HTTP / WebSocket、"**no runtime dependencies beyond the standard library**"、Conan2/vcpkg/CMake `find_package`、自称 server conformance 272/272；同时自述"**not an official MCP SDK** unless accepted by the MCP maintainers"、"release-candidate quality" | C++ 侧唯一看起来认真对齐新规范的候选；"无第三方运行时依赖"这一点对内嵌体积最有价值 | https://github.com/caomengxuan666/cxxmcp （README 原文） | 版本号未在 README 中给出，**未确认**发布版本/日期 |
| `cpp-mcp`（C++，MIT）：实现"conforming to the **2024-11-05** basic protocol specification"，HTTP+SSE 与 stdio，nlohmann-json 风格 API，需要 SSL 选项时链 OpenSSL | 停留在两代之前的协议版本，聚合/现代语义全无 | https://github.com/adithyabsk/cpp-mcp （README 原文） | 未标注版本，**未确认** |
| 检索"zig mcp"命中的 `zig-wasm/zig-mcp` 是"提供 Zig 标准库文档的 **MCP server**"（npm 包），不是 Zig 写的 MCP SDK | Zig 侧**未找到 SDK** | https://www.npmjs.com/package/zig-mcp ・ https://github.com/zig-wasm/zig-mcp | 未确认（仅检索层面） |

---

## 对技术选型的影响

### 建议 1：MCP 层自研，语言侧继续 Rust，且明确"单线程 + 无 async 运行时"（确定性：高）

理由不是"省事"，而是**依赖结构不可裁剪**：rmcp 的 tokio/tracing/serde_json 是硬依赖，`rust-mcp-sdk` 同样以 tokio 为底座，`mcp-sdk-rs` 虽小但已停更且自述不可用于生产。而 stdio 内嵌聚合 Server 需要的协议面（见建议 2）**不含 HTTP 服务端、TLS、OAuth、SSE、并发会话**，这些正是 SDK 体积的主要来源。**【推断】** 按此裁剪，一个只依赖 `serde`/`serde_json`（或自写 JSON 解析）+ 标准库线程的 Rust 实现，`x86_64-unknown-linux-musl` + `--release` + `strip` 落在数百 KB 量级是合理的；但**本笔记没有做构建实测**，见"未确认项"。

### 建议 2：`2026-07-28` 现代版的"最小必须集"（确定性：高，逐条有规范条文）

1. **分帧**：stdin/stdout 按行读，一行一条 JSON-RPC；拒绝含内嵌换行的消息；stdout 只写 MCP 消息，日志一律 stderr。
2. **JSON-RPC 2.0 基本盘**：请求 id 非 null；响应 `result` 或 `error`；notification 无 id；错误码 `-32700/-32600/-32601/-32602/-32603`。
3. **`_meta` 校验**：缺 `io.modelcontextprotocol/protocolVersion` 或 `io.modelcontextprotocol/clientCapabilities` → `-32602`；版本不受支持 → `-32022` 并给出 `supported` 列表。
4. **`server/discover`（MUST）**：返回 `supportedVersions`、`capabilities.tools`、`_meta['io.modelcontextprotocol/serverInfo']`、`ttlMs`、`cacheScope`。
5. **`tools/list`**：`resultType:"complete"` + `tools[]` + `ttlMs` + `cacheScope`，确定性排序；`inputSchema` **原样透传**插件清单里的 JSON Schema（不必自建校验器）。
6. **`tools/call`**：按名路由到插件 IPC；成功返回 `content[]`（+可选 `structuredContent`），执行失败返回 `isError:true`；未知工具返回 `-32602`。
7. **`notifications/cancelled`**：收到后停止该 id 的工作且不再发任何消息（SHOULD 级，但实现很便宜）。
8. **工具名消歧**：按规范建议用 `<pluginId>.<toolName>` 前缀；同时保留原名到路由的映射表。

### 建议 3：必须做 dual-era，把 legacy 握手当作"一个可选分支"（确定性：中高）

- **判定**：stdio 上首个请求是 `initialize` → legacy；带现代 `_meta` → modern。也可按规范推荐先手 `server/discover` 探测，但作为服务端只需被动分支。
- **legacy 分支要补的**：`initialize` 结果（`protocolVersion`、`capabilities`、`serverInfo`）、`notifications/initialized`（忽略即可）、`ping`（回空 result）、结果不带 `resultType`、`tools/list` 不需要 `ttlMs`/`cacheScope`、列表变更用裸 `notifications/tools/list_changed`。
- **代价**：多约一份分支逻辑与两条测试路径；收益是覆盖 2026-11 仍在支持窗口内的 v1 代客户端（官方 TS v1.x 支持至 2026-07-27 后至少六个月）。
- 若明确只服务自家客户端（PluginManager 自带 MCP 客户端），可以砍掉 legacy——**但要在文档里写死这个决定**，否则以后接 IDE/桌面客户端会返工。

### 建议 4：插件热插拔与 `tools/list` 语义必须二选一，现在就定（确定性：高，属于规范硬约束）

- **方案 A（简单）**：`tools:{listChanged:false}`，工具集合视为"进程生命周期内稳定"。装卸插件后不主动通知；客户端在 `ttlMs` 过期后重新 list，或重连才看到变化。
- **方案 B（正确但要长请求）**：`tools:{listChanged:true}` + 实现 `subscriptions/listen`（长驻请求 + `notifications/subscriptions/acknowledged` + 带 `subscriptionId` 的 `notifications/tools/list_changed`）。这要求读写循环**必须能并发**（长请求不能阻塞后续 `tools/call`），是自研实现里最容易被低估的一处架构成本。
- 建议：先 A（与"最小实现"目标一致），把 B 记成显式技术债，并在 `tools/list` 的 `ttlMs` 上用较短值（如 30–60s）补偿。

### 建议 5：HTTP 传输现在不做，将来要做也别上 rmcp 全套（确定性：中）

- 内嵌场景下 PluginManager 由宿主以 stdio 拉起，Streamable HTTP 没有收益，只增加 HTTP server + SSE + session/header 校验 + (若暴露) OAuth 的面。
- 若将来必须要 HTTP：`2026-07-28` 的 Streamable HTTP 已无 GET 流、无 session、无 SSE 恢复，比过去简单；可评估 `cxxmcp`（无第三方运行时依赖）或自研（只需 POST + 单请求 SSE 流，外加三个请求头 `MCP-Protocol-Version`（与 body `_meta` 不一致时 `-32020 HeaderMismatch`）、`Mcp-Method`、`Mcp-Name` 的生成与校验）。
- 明确不推荐：为 HTTP 引入 rmcp / rust-mcp-sdk。

### 建议 6：体积与维护风险的对照结论（确定性：中高）

| 路线 | 体积 | 维护风险 | 适配本项目的判断 |
| --- | --- | --- | --- |
| 自研最小 stdio 层（+ dual-era） | 【推断】数百 KB 级，可控 | 协议细节自己兜；规范改版要跟（但 stdio 子集稳定，2026-07-28 反而是删减） | **推荐** |
| `rmcp` | 【推断】MB 级（tokio+tracing+serde 栈） | 最低（官方 Tier 1，月更，已对齐 2026-07-28） | 与 ≤1MB 目标不相容 |
| `rust-mcp-sdk` | 【推断】MB 级 | 中（社区项目，活跃） | 同上 |
| `mcp-sdk-rs` | 小（1.5k 行） | 高（自述 WIP、停更于 2025-11、无 Streamable HTTP、无现代版） | 不建议 |
| `cxxmcp` | 未知；依赖仅标准库（对体积友好） | 中高（"release-candidate"、非官方、无公开版本号可核对） | 若真要 C++ 才考虑 |
| `cpp-mcp` | 未知 | 高（协议停在 2024-11-05） | 不建议 |
| LiteMCP（任一） | 不适用 | npm 版已停维；Rust 版只做 HTTP 且过新 | 不作为依赖 |

### 建议 7：给"内嵌聚合 Server"补三条聚合器专属设计（超出规范，但必须有，确定性：中）

1. **命名空间与冲突**：`<pluginId>.<toolName>`；插件重装导致工具消失时，旧名调用返回 `-32602` 而不是路由到别的插件。
2. **错误映射**：插件崩溃/超时/协议错 → `isError:true` 且文案可自纠（规范把"可自纠"作为 `isError:true` 的语义）；只有"调用方给错工具名/参数"才用 `-32602`。
3. **IPC 子进程的 stderr 与生命周期**：插件 stderr 绝不能进 PluginManager 的 stdout（会污染 MCP 帧）；插件退出即从路由表摘除，但**不要**因此改 `tools/list`（见建议 4）。

---

## 未确认项

1. **Rust 自研最小实现的真实二进制体积未实测**：本机未做 musl 构建，"数百 KB 级"是【推断】，不是测量值。要把它变成事实，需要在 `x86_64-unknown-linux-musl` 下用相同 feature 集分别构建"空 main + serde_json"与"完整 MCP 子集"两版做差值。
2. **`rmcp` 的实际二进制增量未实测**：只核对了源码行数与依赖清单，没有构建 `rmcp` + `transport-io` 的最小 server 看 `strip` 后的体积。
3. **`rmcp` 3.x 是否 dual-era（支持 legacy `initialize`）未确认**：crates.io/docs.rs 元数据与 README 未标注；`rmcp` 3.0.0 恰在 2026-07-28 发布，看起来是跟改版对齐，但"是否保留 2025-11-25 兼容路径"需读源码或 changelog 才能定论。
4. **2026-11 真实客户端的 era 分布未确认**：只确认官方 TS SDK v1.x 仍在维护窗口内（≥2026-07-27 起六个月），没有各宿主（IDE/桌面客户端）实际协商版本的样本。
5. **`cxxmcp` / `cpp-mcp` 的维护状态（最近提交、是否归档、发布版本号、许可证文件、实际体积）未确认**：GitHub REST API 在本环境被限流（`403 rate limit exceeded`），只能读 README 原文，无法取 `archived`/`pushed_at`/`license` 字段。
6. **Zig 侧是否存在非公开/未检索到的 SDK 未确认**：仅确认"zig-mcp"是 Zig 文档服务端而非 SDK，不能断言"Zig 生态完全没有实现"。
7. **`litemcp`（crates.io）是否与 `wong2/litemcp` 有血统关系未确认**：两者描述、语言、架构均不同，代码未比对；按现有证据应视为同名不同项目。
8. **插件清单里 `inputSchema` 的 JSON Schema 方言是否需要校验未定**：2026-07-28 要求客户端/服务端 MUST 支持 2020-12 且 MUST 按声明方言校验；本方案选择"原样透传、不校验"，这对聚合器是合理取舍（校验应由插件侧负责），但严格意义上不完全满足"Servers MUST validate all tool inputs"的安全要求，需要在设计文档里显式豁免。
9. **`subscriptions/listen` 在 stdio 上的并发模型未做原型验证**：方案 B 需要"长请求 + 并发写 stdout"的序列化保证（单一 writer），这是自研实现的主要复杂度来源，未写代码验证。
