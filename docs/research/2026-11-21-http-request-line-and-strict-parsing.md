# 请求行语法、严格解析边界与 414/431（RFC 9112 / 9110 一手核对）

> 调研日期：2026-11-21
> 问题：#251 要判定 svcd 手写 HTTP/1.1 解析器里那些「宽松点」哪些是规范**要求必须拒绝**的、414 在什么条件下该出现、请求行上限与整个头上限在参考实现里是一个旋钮还是两个。

**取证方式**：全部一手来源 —— `rfc-editor.org` 的 RFC 9112 / 9110 / 6585 原文（逐段抄录，不用二手总结）；nginx、Apache httpd、Node.js、Go、hyper、httparse 的官方文档页或官方 API 文档页。
**没取到的**：本沙箱 DNS 拦掉了 `github.com`、`raw.githubusercontent.com`、`gitbox.apache.org`、`learn.microsoft.com`、`sources.debian.org`，所以 **httpd 源码里的状态码**和 **IIS 的 requestLimits 默认值**没能逐字核对，统一进「未确认项」，正文相应处标 [推断]。

**不重复的内容**：`parse` / `parse_head` / `header_end` 的现状代码行为、`max_header_bytes` 的键与 floor/天花板、431 `headers_too_large` 的拒绝路径与 drain（#106）已由调用方读过源码确认，本笔记只做规范与实现口径的对照。

## 结论（先给答案）

1. **请求行语法是「恰好一个 SP」，这是规范性语法；但同一节又用 MAY 允许接收方按任意空白切分。** `request-line = method SP request-target SP HTTP-version`；`SP` 是 ABNF 里的字面量，不是「分隔符集合」。紧随其后的一段原文明确给出容错许可（SP / HTAB / VT / FF / 裸 CR 都当 SP 用），并同时警告这会导致 request smuggling。→ 「用 `split_whitespace()` 切请求行」这件事本身在规范许可范围内；「切完只用前两个 token、后面的静默扔掉」不在许可范围内。[规范]
2. **多余 token、两段式请求行没有任何容错条款。** 规范里唯一能套的是 RFC 9112 §2.2 的兜底句：不匹配 HTTP-message 文法（且不属于前文列出的 robustness 例外）→ **SHOULD** 回 400 并关连接。所以 `GET / HTTP/1.1 extra` 与 `GET /` 都应回 400，而不是当 HTTP/1.1 正常服务。[规范]
3. **三个 MUST 级拒绝点，svcd 目前全部违反：** header 名与冒号之间的空白 → **MUST** 400（§5.1）；obs-fold → **MUST** 要么 400、要么替换成一个或多个 SP，**静默丢弃不在允许选项里**（§5.2）；缺 `Host` / 多个 `Host` / 非法 `Host` → **MUST** 400（§3.2）。[规范]
4. **裸 LF 是 MAY 级容错，不是必须拒绝。** 原文是 "a recipient MAY recognize a single LF as a line terminator"（§2.2）。所以「只认 CRLF、裸 LF 永不结束」是合法的实现选择，但代价是：它把一个语法问题报成了长度问题（431），而 431 的语义是「字段太大」。[规范]
5. **裸 CR 是 MUST 级**：发送方 MUST NOT 产生；接收方 **MUST** 要么把该元素判为无效、要么把每个裸 CR 替换成 SP（§2.2）。「保留裸 CR 当普通字符继续解析」不可选。[规范]
6. **414 的规范触发条件只有一个：request-target 的长度**，而且是 **MUST** —— "A server that receives a request-target longer than any URI it wishes to parse MUST respond with a 414 (URI Too Long)"（§3）。方法名超长是 **SHOULD 501**，不是 414、也不是 400。规范同时给了唯一一个数字：**RECOMMENDED 至少支持 8000 octets 的请求行**。[规范]
7. **414 与 431 的优先级，规范没有规定。** 更关键的事实是：**431 不在 RFC 9110 里**（9110 全文不含 "431" 字样），它由 RFC 6585 §5 定义；RFC 9110 §5.4 对「字段/字段集太大」只要求 **"an appropriate 4xx"**，并点名 431 之外的任何 4xx 也能满足字面要求。→ 只用一个「整头上限」旋钮的实现，请求行超长必然报 431；只有把请求行与字段分开限制的实现，才会出现 414。[规范] + [推断]（优先级缺失是规范事实，「必然报 431」是结构推断）
8. **非法字符 → 400，不 → 414。** 414 的定义只关于长度（"the target URI is longer than the server is willing to interpret"）；request-target 含空白/非法字符属于 invalid request-line，规范给的是 **400 或 301 重定向**，且 SHOULD NOT 自动改正后继续处理（§3.2）。`#` 不属于 request-target。[规范]
9. **业界默认值分两派**（详见 E 节表）：**两个旋钮**的是 nginx（请求行 ≤ 一个 8 KiB buffer，字段各 ≤ 一个 buffer，超了分别回 414 / 400，nginx **不用 431**）和 Apache httpd（`LimitRequestLine` 8190 / `LimitRequestFieldSize` 8190 / `LimitRequestFields` 100）；**一个旋钮**的是 Go（`MaxHeaderBytes` 1 MiB，文档明说 "including the request line"，超限 431）、Node（`--max-http-header-size` 16 KiB，超限 431）、hyper（`max_header_size` 含 start line，默认未设→回落 `max_buf_size` ~400 KiB，超限 431）。httparse 本身无字节上限，上限全由调用方 buffer / header 数组长度决定。[规范/实测混合，逐条见 E]
10. **HTTP/1.0 默认不持久、pipelining 仍是 MAY 且未被废弃；HEAD/1xx/204/304 无 body 是 framing 规则，不是「内容为空」。** 9112 §9.3 的四个判定分支里，HTTP/1.0 只有带 `keep-alive` 且接收方愿意才持久；§9.3.2 原文仍是 "MAY \"pipeline\""，RFC 9112 没有任何把它降级为 deprecated 的措辞。9112 §6.3 第 1 条：HEAD 响应和 1xx/204/304 响应「always terminated by the first empty line after the header fields」，因此不可能含 body；9110 §8.6：**1xx 与 204 MUST NOT 发 Content-Length**，304 可发但值必须等于对同一请求的 200 会发的字节数。[规范]

## 证据

### A. RFC 9112 §3 请求行的语法与容错

| 结论 | 依据（原文/条款） | 来源 URL | 日期或版本 |
|---|---|---|---|
| 请求行 ABNF 是 method SP target SP version，SP 为字面量 | "A request-line begins with a method token, followed by a single space (SP), the request-target, and another single space (SP), and ends with the protocol version."；`request-line = method SP request-target SP HTTP-version` [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3 | RFC 9112，2022-06（STD 99） |
| 「恰好一个 SP」是语法要求，但规范**自己**给了按空白切分的 MAY，并列出被认可的空白 octet 集合 | "Although the request-line grammar rule requires that each of the component elements be separated by a single SP octet, recipients MAY instead parse on whitespace-delimited word boundaries and, aside from the CRLF terminator, treat any form of whitespace as the SP separator while ignoring preceding or trailing whitespace; such whitespace includes one or more of the following octets: SP, HTAB, VT (%x0B), FF (%x0C), or bare CR." [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3 | 同上 |
| 同一段立刻指出宽松解析的安全代价 | "However, lenient parsing can result in request smuggling security vulnerabilities if there are multiple recipients of the message and each has its own unique interpretation of robustness (see Section 11.2)." [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3 | 同上 |
| 请求行长度没有预定义上限；方法超长 → SHOULD 501；**target 超长 → MUST 414** | "HTTP does not place a predefined limit on the length of a request-line … A server that receives a method longer than any that it implements SHOULD respond with a 501 (Not Implemented) status code. A server that receives a request-target longer than any URI it wishes to parse MUST respond with a 414 (URI Too Long) status code" [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3 | 同上 |
| 规范给的唯一数字：请求行至少 8000 octets | "It is RECOMMENDED that all HTTP senders and recipients support, at a minimum, request-line lengths of 8000 octets." [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3 | 同上 |
| 文法外（含多余 token、缺 version、缺 SP）的兜底是 SHOULD 400 + 关连接 | "When a server … receives a sequence of octets that does not match the HTTP-message grammar aside from the robustness exceptions listed above, the server SHOULD respond with a 400 (Bad Request) response and close the connection." [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-2.2 | 同上 |
| method 是 token；HTTP-version 大小写敏感，形如 `HTTP/DIGIT.DIGIT` | `method = token`；"HTTP-version is case-sensitive."；`HTTP-version = HTTP-name "/" DIGIT "." DIGIT`，`HTTP-name = %s"HTTP"` [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3.1 · ...#section-2.3 | 同上 |
| 版本号语法合法但不受支持 → 505，不是 400 | "The 505 (HTTP Version Not Supported) status code indicates that the server does not support, or refuses to support, the major version of HTTP that was used in the request message." [规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-15.6.6 | RFC 9110，2022-06 |
| 请求行前允许吞掉空行（robustness） | "In the interest of robustness, a server that is expecting to receive and parse a request-line SHOULD ignore at least one empty line (CRLF) received prior to the request-line." [规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-2.2 | RFC 9112 |

### B. 哪些宽松点必须拒绝、哪些只是 MAY 容错

| 结论 | 依据（原文/条款） | 来源 URL | 日期或版本 |
|---|---|---|---|
| 裸 LF 行结束：**MAY 容忍**，不是必须拒绝 | "Although the line terminator for the start-line and fields is the sequence CRLF, a recipient MAY recognize a single LF as a line terminator and ignore any preceding CR."（§2.2）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-2.2 | RFC 9112 |
| 裸 CR：发送 MUST NOT；接收 **MUST** 判无效或替换为 SP | "A sender MUST NOT generate a bare CR (a CR character not immediately followed by LF) within any protocol elements other than the content. A recipient of such a bare CR MUST consider that element to be invalid or replace each bare CR with SP before processing the element or forwarding the message."（§2.2）[规范] | 同上 | 同上 |
| obs-fold：**MUST** 400 或 **MUST** 替换为 SP —— 丢弃不是选项 | "A server that receives an obs-fold in a request message that is not within a "message/http" container MUST either reject the message by sending a 400 (Bad Request) … or replace each received obs-fold with one or more SP octets prior to interpreting the field value or forwarding the message downstream."（§5.2）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-5.2 | 同上 |
| start-line 与首个 header 之间的空白（首行即以空白开头）：**MUST** 拒绝或整行忽略 | "A recipient that receives whitespace between the start-line and the first header field MUST either reject the message as invalid or consume each whitespace-preceded line without further processing of it (i.e., ignore the entire line, along with any subsequent lines preceded by whitespace, until a properly formed header field is received or the header section is terminated)."（§2.2）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-2.2 | 同上 |
| ↑ 这两条不是重复：第一条管「header 之后以空白开头的行」（= obs-fold，丢弃不合规）；第二条管「start-line 之后立刻以空白开头的行」（整行忽略被明确允许） | 两条原文并列，判定依据是「前一行是 start-line 还是 header」[推断] | 同上 | 同上 |
| header 名与冒号之间的空白：**MUST 400**，且点明是历史漏洞 | "No whitespace is allowed between the field name and colon. … A server MUST reject, with a response status code of 400 (Bad Request), any received request message that contains whitespace between a header field name and colon."（§5.1）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-5.1 | 同上 |
| header 名本身必须是 token；非 token 字符（如 `@`、`(`）只有 §2.2 的 SHOULD 400 兜底，没有单独的 MUST | `field-name = token`（9110 §5.1）；非法字符不在 §5.1 那段 MUST 的射程内，落回 §2.2 的 SHOULD 400 [规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-5.1 | RFC 9110 |
| 缺 Host / 多个 Host / 非法 Host：**MUST 400** | "A server MUST respond with a 400 (Bad Request) status code to any HTTP/1.1 request message that lacks a Host header field and to any request message that contains more than one Host header field line or a Host header field with an invalid field value."（§3.2）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2 | RFC 9112 |
| 字段/字段集超限：**MUST 回一个 4xx**，规范只写 "appropriate 4xx"，没写死 431 | "A server that receives a request header field line, field value, or set of fields larger than it wishes to process MUST respond with an appropriate 4xx (Client Error) status code. Ignoring such header fields would increase the server's vulnerability to request smuggling attacks"（9110 §5.4）[规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-5.4 | RFC 9110 |
| 其余非法构造的默认许可是「MAY 尝试恢复」 | "Unless noted otherwise, a recipient MAY attempt to recover a usable protocol element from an invalid construct."（9110 §2.4）[规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-2.4 | RFC 9110 |
| 长度方面的互操作下限只有「至少能解析自己会发出的长度」 | "At a minimum, a recipient MUST be able to parse and process protocol element lengths that are at least as long as the values that it generates for those same protocol elements in other messages."（9110 §2.3）[规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-2.3 | RFC 9110 |

### C. 414 与 431 的规范定义、优先级

| 结论 | 依据（原文/条款） | 来源 URL | 日期或版本 |
|---|---|---|---|
| 414 只讲「target URI 比服务器愿意解释的更长」 | "The 414 (URI Too Long) status code indicates that the server is refusing to service the request because the target URI is longer than the server is willing to interpret."；并说明该码可被启发式缓存 [规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-15.5.15 | RFC 9110 |
| **431 不在 RFC 9110 里**；它由 RFC 6585 §5 定义（9110 全文无 "431" 字样，状态码表里也没有 431 行） | 9110 §15.5 的序列是 …15.5.19 418、15.5.20 421、15.5.21 422 …，无 431 [规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-15.5 · https://www.rfc-editor.org/rfc/rfc6585.html#section-5 | RFC 9110 / RFC 6585（2012-04） |
| 431 的语义是「字段太大」，且同时覆盖「整组太大」与「单个字段太大」 | "The 431 status code indicates that the server is unwilling to process the request because its header fields are too large. The request MAY be resubmitted after reducing the size of the request header fields. It can be used both when the set of request header fields in total is too large, and when a single header field is at fault."；"Responses with the 431 status code MUST NOT be stored by a cache." [规范] | https://www.rfc-editor.org/rfc/rfc6585.html#section-5 | RFC 6585 §5 |
| **规范没有给 414 / 431 的优先级**。两者射程不重叠：414 判的是 target 长度（9112 §3 的 MUST），431 判的是字段区大小（6585 §5）；9110 §5.4 只要求 4xx。请求行与整头同时超限时，两条都能被满足，无条款排序 [规范] | 三条原文并列，未见任何 ordering 措辞 [推断：判定为「规范缺失」] | 同上三处 | — |
| 业界结果：**一个旋钮 → 431；两个旋钮 → 414** | nginx 请求行超 one buffer → 414、字段超 → 400；Go/Node/hyper 只有整头上限 → 431（见 E 节逐条来源）[推断] | 见 E 节 | 2026-11 抓取 |

### D. 414 vs 400 的边界：非法字符该回哪个

| 结论 | 依据（原文/条款） | 来源 URL | 日期或版本 |
|---|---|---|---|
| 414 的定义里只有长度，没有「字符非法」这一维 | 9110 §15.5.15 全文只提 "longer than the server is willing to interpret" [规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-15.5.15 | RFC 9110 |
| request-target 里不允许空白；invalid request-line 的处置是 **400 或 301**，且 SHOULD NOT 自动改正后继续 | "No whitespace is allowed in the request-target."；"Recipients of an invalid request-line SHOULD respond with either a 400 (Bad Request) error or a 301 (Moved Permanently) redirect with the request-target properly encoded. A recipient SHOULD NOT attempt to autocorrect and then process the request without a redirect, since the invalid request-line might be deliberately crafted to bypass security filters along the request chain."（§3.2）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-3.2 | RFC 9112 |
| `#` 不属于 request-target（fragment 是客户端侧概念） | "The target URI excludes the reference's fragment component, if any, since fragment identifiers are reserved for client-side processing."（9110 §7.1）；§4.2.5 注明 fragment 不在 http(s) scheme 的 ABNF 里 [规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-7.1 | RFC 9110 |
| 控制字符与原始非 ASCII 字节：按 octet 解析 + URI 语法只允许 pchar/percent-encoded → 属文法违例 → 400 | 9112 §2.2 "A recipient MUST parse an HTTP message as a sequence of octets in an encoding that is a superset of US-ASCII"；§3.2.1 只给 `origin-form = absolute-path [ "?" query ]`，逐字符合规性由 RFC 3986 定义 [规范] + [推断]（9112 未逐字符列出非法集合） | https://www.rfc-editor.org/rfc/rfc9112.html#section-2.2 · ...#section-3.2.1 | RFC 9112 |
| 长度与非法同时命中时，规范不规定判定次序；两种码都能找到依据（414 有 MUST 但前提是「你先决定它太长不再解析」，400 有 SHOULD） | 两条原文并列，无 ordering [推断] | 同上 | — |

### E. 参考实现的默认值：请求行上限与整头上限是一个旋钮还是两个

| 结论 | 依据（原文/数值） | 来源 URL | 日期或版本 |
|---|---|---|---|
| **nginx：两个旋钮，且从不用 431。** `large_client_header_buffers` 默认 **`4 8k`**；`client_header_buffer_size` 默认 **`1k`**。请求行不能超过「一个 buffer」→ **414**；单个 header field 也不能超过「一个 buffer」→ **400** | "Sets the maximum number and size of buffers used for reading large client request header. A request line cannot exceed the size of one buffer, or the 414 (Request-URI Too Large) error is returned to the client. A request header field cannot exceed the size of one buffer as well, or the 400 (Bad Request) error is returned to the client." [规范/一手文档] | http://nginx.org/en/docs/http/ngx_http_core_module.html#large_client_header_buffers · ...#client_header_buffer_size | nginx.org 当前文档，2026-11 抓取 |
| **Apache httpd：三个独立旋钮。** `LimitRequestLine` 默认 **8190**；`LimitRequestFieldSize` 默认 **8190**；`LimitRequestFields` 默认 **100** | 三条指令的 `Default:` 行原文即上述数字 [实测：文档 `Default:` 行] | https://httpd.apache.org/docs/2.4/mod/core.html#limitrequestline · ...#limitrequestfieldsize · ...#limitrequestfields | httpd 2.4 文档，2026-11 抓取 |
| httpd 超限时回什么码：官方指令页**没写**状态码（`core.html` 全文 0 处 "414"） → 常见口径是请求行超限 414、字段超限 400，但本轮没有一手核对 | 文档缺席 + 源码站不可达 **[未取到]** [推断] | 同上（缺席事实） | — |
| **Go `net/http`：一个旋钮，且明确包含请求行。** `DefaultMaxHeaderBytes = 1 << 20`（1 MiB）；超限回 **431** | "MaxHeaderBytes controls the maximum number of bytes the server will read parsing the request header's keys and values, including the request line. It does not limit the size of the request body. If zero, DefaultMaxHeaderBytes is used."；`DefaultMaxHeaderBytes = 1 << 20`；`StatusRequestHeaderFieldsTooLarge = 431 // RFC 6585, 5` [实测：文档源码块] | https://pkg.go.dev/net/http#Server · https://pkg.go.dev/net/http#DefaultMaxHeaderBytes | pkg.go.dev 当前版，2026-11 抓取 |
| Go 没有单独的 URI / request-line 上限；请求行只能靠 `MaxHeaderBytes` 兜住（同一条文案把它算进 header 字节） | 同上一条文案 "including the request line" [规范/一手文档] | 同上 | 同上 |
| **Node.js：一个旋钮。** `--max-http-header-size` 默认 **16 KiB**（v13.13.0 起由 8 KiB 改为 16 KiB）；超限回 **431** | "Specify the maximum size, in bytes, of HTTP headers. Defaults to 16 KiB."；"v13.13.0 Change maximum default size of HTTP headers from 8 KiB to 16 KiB."；"v12.0.0 The default behavior will return a 431 Request Header Fields Too Large if an HPE_HEADER_OVERFLOW error occurs." [实测：文档原文] | https://nodejs.org/api/cli.html#--max-http-header-sizesize · https://nodejs.org/api/http.html | nodejs.org 当前文档，2026-11 抓取 |
| **hyper（1.12，h1 server）：一个旋钮，含 start line。** `max_header_size` 默认 **None**，未设时回落 `max_buf_size`（默认 **~400 KiB**，最小值 8192，传入更小会 panic）；超限回 **431** 并关连接 | "Set the maximum size of request headers (including the start line) in bytes. If the client sends headers exceeding this limit, the server responds to the client with "431 Request Header Fields Too Large" and closes the connection. If not configured, then the max_buf_size will naturally be reached and applied. … Default is None."；`max_buf_size`: "Set the maximum buffer size for the connection. Default is ~400kb. … The minimum value allowed is 8192." [实测：docs.rs 原文] | https://docs.rs/hyper/latest/hyper/server/conn/http1/struct.Builder.html#method.max_header_size · ...#method.max_buf_size | hyper 1.12.0 |
| hyper 把「多个空格当分隔符」做成显式开关，**默认关闭** | `allow_multiple_spaces_in_request_line_delimiters`："Set whether multiple spaces are allowed as delimiters in request lines. Default is false." [实测] | 同上 `#method.allow_multiple_spaces_in_request_line_delimiters` | hyper 1.12.0 |
| **httparse：无字节上限。** 解析器只吃调用方给的 buffer 和 header 数组；数组放不下 → `Error::TooManyHeaders` | `TooManyHeaders`："Parsed more headers than provided buffer can contain." [实测：docs.rs 源码页] | https://docs.rs/httparse/latest/httparse/ | httparse 1.10.1 |
| **IIS：两个旋钮（`maxUrl` / `maxFieldLength`），默认值本轮未核实** | 见「未确认项」**[未取到]** [推断] | https://learn.microsoft.com/en-us/iis/configuration/system.webServer/security/requestFiltering/requestLimits/ | 未能抓取（DNS 不解析 / 30 s 超时） |

**一个旋钮 vs 两个旋钮，速查**

| 实现 | 请求行独立上限 | 整头 / 字段上限 | 请求行超限回什么 | 整体为单旋钮？ |
|---|---|---|---|---|
| nginx | 有：一个 buffer（默认 8 KiB，`large_client_header_buffers` 的 size） | 有：单个字段 ≤ 一个 buffer；总量靠 buffer 数量 | 414 | 否（两旋钮） |
| Apache httpd 2.4 | 有：`LimitRequestLine 8190` | 有：`LimitRequestFieldSize 8190` + `LimitRequestFields 100` | 414（文档未写，**[未取到]** [推断]） | 否（三旋钮） |
| Go net/http | 无 | 有：`MaxHeaderBytes` 1 MiB（含请求行） | 431 | 是 |
| Node.js | 无 | 有：`--max-http-header-size` 16 KiB | 431 | 是 |
| hyper 1.x | 无 | 有：`max_header_size` → `max_buf_size` ~400 KiB（含 start line） | 431 | 是 |
| httparse | 无 | 无（调用方决定） | 由调用方决定 | — |
| IIS | 有：`maxUrl`（**[未取到]** [推断] 4096） | 有：`maxFieldLength`（**[未取到]** [推断] 4096）+ `maxQueryString`（**[未取到]** [推断] 2048） | 404.14 / 404.15（**[未取到]** [推断]） | 否（两旋钮） |

### F. HTTP/1.0、keep-alive、pipelining

| 结论 | 依据（原文/条款） | 来源 URL | 日期或版本 |
|---|---|---|---|
| HTTP/1.1 默认持久；HTTP/1.0 只有在 `keep-alive` 存在且接收方愿意时才持久，否则关闭 | "If the received protocol is HTTP/1.1 (or later), the connection will persist after the current response; else, If the received protocol is HTTP/1.0, the "keep-alive" connection option is present, either the recipient is not a proxy or the message is a response, and the recipient wishes to honor the HTTP/1.0 "keep-alive" mechanism, the connection will persist after the current response; otherwise, The connection will close after the current response."（§9.3）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-9.3 | RFC 9112 |
| 不支持持久连接时，服务器 **MUST** 在每个非 1xx 响应里发 `Connection: close` | "A server that does not support persistent connections MUST send the "close" connection option in every response message that does not have a 1xx (Informational) status code."；客户端侧对应 MUST 在每请求里发 `close` [规范] | 同上 | 同上 |
| 持久连接的前提是消息自带定界长度；服务器 **MUST** 读完整个请求 body 或关连接 | "In order to remain persistent, all messages on a connection need to have a self-defined message length … A server MUST read the entire request message body or close the connection after sending its response"（§9.3）[规范] | 同上 | 同上 |
| pipelining **仍是 MAY，RFC 9112 没有废弃它**；服务器可并行处理但 **MUST** 按序回响应 | "A client that supports persistent connections MAY "pipeline" its requests (i.e., send multiple requests without waiting for each response). A server MAY process a sequence of pipelined requests in parallel if they all have safe methods …, but it MUST send the corresponding responses in the same order that the requests were received."（§9.3.2）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-9.3.2 | 同上 |
| 代理 **MUST NOT** 与 HTTP/1.0 客户端保持持久连接 | "A proxy server MUST NOT maintain a persistent connection with an HTTP/1.0 client"（§9.3）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-9.3 | 同上 |

### G. HEAD / 204 / 304 不得带 body；Content-Length 怎么写

| 结论 | 依据（原文/条款） | 来源 URL | 日期或版本 |
|---|---|---|---|
| 语义层：HEAD 响应「never include content」；1xx、204、304「do not include content」 | "Responses to the HEAD request method (Section 9.3.2) never include content"；"All 1xx (Informational), 204 (No Content), and 304 (Not Modified) responses do not include content."（§6.4.1）[规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-6.4.1 | RFC 9110 |
| framing 层：这些响应「always terminated by the first empty line after the header fields」，因此**不可能**含 body 或 trailer | "Any response to a HEAD request and any response with a 1xx (Informational), 204 (No Content), or 304 (Not Modified) status code is always terminated by the first empty line after the header fields, regardless of the header fields present in the message, and thus cannot contain a message body or trailer section."（§6.3 第 1 条）[规范] | https://www.rfc-editor.org/rfc/rfc9112.html#section-6.3 | RFC 9112 |
| 304 由 header 区结束界定，不含 content 与 trailer | "A 304 response is terminated by the end of the header section; it cannot contain content or trailers."（§15.4.5）[规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-15.4.5 | RFC 9110 |
| **204：MUST NOT 发 Content-Length**；1xx 同样；2xx CONNECT 同样 | "A server MUST NOT send a Content-Length header field in any response with a status code of 1xx (Informational) or 204 (No Content). A server MUST NOT send a Content-Length header field in any 2xx (Successful) response to a CONNECT request"（§8.6）[规范] | https://www.rfc-editor.org/rfc/rfc9110.html#section-8.6 | RFC 9110 |
| HEAD：**可**发 Content-Length，但值必须等于同一请求用 GET 时会发出的字节数 | "A server MAY send a Content-Length header field in a response to a HEAD request (Section 9.3.2); a server MUST NOT send Content-Length in such a response unless its field value equals the decimal number of octets that would have been sent in the content of a response if the same request had used the GET method."（§8.6）[规范] | 同上 | 同上 |
| 304：**可**发 Content-Length，但值必须等于对同一请求的 200 会发的字节数 | "A server MAY send a Content-Length header field in a 304 (Not Modified) response to a conditional GET request (Section 15.4.5); a server MUST NOT send Content-Length in such a response unless its field value equals the decimal number of octets that would have been sent in the content of a 200 (OK) response to the same request."（§8.6）[规范] | 同上 | 同上 |

## 未确认项

1. **[未取到] IIS 的 `requestLimits` 默认值**（`maxUrl` / `maxFieldLength` / `maxQueryString`）与超限时回 404.14 / 404.15 的官方出处：`learn.microsoft.com` 在本次沙箱内 DNS 不解析、`web_fetch` 三次 30 s 超时；`iis.net` 301 到 `docs.microsoft.com` 再被跨域重定向拒绝；本机也没有 `%windir%\System32\inetsrv\config\schema\IIS_schema.xml`（未装 IIS）。业内通行值是 4096 / 4096 / 2048，但本轮**未逐字核对**，一律标 [推断]。
2. **[未取到] Apache httpd 超限时的确切状态码**：`core.html` 三条指令页全文没有出现 "414"，httpd 源码站（`github.com`、`raw.githubusercontent.com`、`gitbox.apache.org`、`svn.apache.org`、`dlcdn.apache.org`、`sources.debian.org`）全部 DNS 不可达，因此「请求行超限 414 / 字段超限 400」是 [推断]。
3. **nginx 内部 494 → 对外 400 的映射**没有核对源码（`hg.nginx.org` 已下线并指向 GitHub），本笔记只用 nginx 官方文档那句「请求行 → 414、字段 → 400」。
4. **Rust `str::split_whitespace` 的空白集合与 §3 MAY 列表的差集**：§3 只认 SP / HTAB / VT / FF / 裸 CR 五个 octet；`split_whitespace` 走的是 Unicode White_Space（会多认 U+00A0 等）。差集需按 `char::is_whitespace` 文档逐项核对，本轮未做。
5. **414 与 400 在「既超长又含非法字符」时的判定次序**：规范确实没写；本笔记只断言「无规定」，没有给出行业共识的实测样本。

## 对 #251 的含义

### 现状逐条定性（每条都能直接变成测试断言）

| svcd 现状 | 规范判定 | 强制级别 | 依据 |
|---|---|---|---|
| 请求行用 `split_whitespace()` 切、只取第 1、2 个 token | 切分本身合规；**丢弃第 3 个起的 token、把 `GET / HTTP/1.1 extra` 当合法**不合规 | SHOULD 400 | 9112 §3（MAY 只覆盖空白切分）＋ §2.2 兜底 400 |
| `parse_head` 用 `nth(2).unwrap_or("HTTP/1.1")`，两段式 `GET /` 当 1.1 服务 | 缺 HTTP-version 是文法违例，不是「默认 1.1」 | SHOULD 400 | 9112 §3（`request-line = method SP target SP HTTP-version`）＋ §2.2 |
| `HTTP/2.0` 之类：语法合法但不受支持 | 语法合法的非 1.x 版本应 505，不能当 1.1、也不能算 400 | SHOULD 505 | 9110 §15.6.6 |
| `split_once(':')` + `trim()`，`Foo : bar` 被接受 | header 名与冒号之间出现空白 | **MUST 400** | 9112 §5.1 |
| 以空白开头的续行因不含 `:` 被静默丢弃 | 若前一行是 header，则是 obs-fold → 丢弃不在允许选项里；若紧接 start-line，则整行忽略被允许 | **MUST**（前者）/ MAY（后者） | 9112 §5.2 vs §2.2 |
| `header_end` 只认 `\r\n\r\n`，裸 LF 请求头永不结束，最终撞 `max_header_bytes` 回 431 | 裸 LF 是 MAY 容忍，拒绝也行；但**用 431 报语法问题**语义错位（431 = 字段太大，RFC 6585 §5） | 规范不禁止「拒绝」，但 431 的语义不对 | 9112 §2.2 ＋ 6585 §5 |
| 只有 `max_header_bytes`（默认 16 KiB）一个旋钮，无请求行/URI 独立上限 | 请求行超长与整头超长被同一个旋钮判成 431；而 target 超长的规范要求是 **MUST 414** | **MUST 414**（target 超长时） | 9112 §3 ＋ 9110 §15.5.15 |
| 无 method / target / header-name token 校验 | method 必须是 token；header 名必须是 token；target 不许空白 | MUST（header 名空白处）/ SHOULD 400（其余文法违例） | 9112 §3.1 §3.2 §5.1 ＋ 9110 §5.1 |
| 无 Host 缺失检查 | 缺 Host / 多 Host / 非法 Host | **MUST 400** | 9112 §3.2 |

### 能直接落成决定的数字与码位

1. **请求行的规范底线是 8000 octets**（RECOMMENDED），nginx 的实际默认是 **8192**（一个 8 KiB buffer），Apache 是 **8190**。三个数字同量级；`max_header_bytes` 当前默认 16 KiB、floor 1024，若把 1024 用在请求行上，**低于规范 RECOMMENDED 的 8000**。
2. **请求行上限必须独立于整头上限，否则拿不到 414。** 只有整头上限的实现（Go 1 MiB / Node 16 KiB / hyper ~400 KiB）在规范上仍然合规，因为它们可以替自己辩称「我按整头判 431」；但 9112 §3 对 target 超长用的是 **MUST 414**，所以「一个旋钮回 431」在 target 单独超长这一类上有被指为不合规的空间。nginx 的做法（请求行 ≤ 一个 buffer → 414；字段 ≤ 一个 buffer → 400）是不引入 431 也能同时满足 §3 与 §5.4 的证据。
3. **431 在 RFC 9110 里不存在**（定义在 RFC 6585 §5），且 6585 §5 明确它同时适用于「整组太大」与「单个字段太大」——所以「单个 header 超过字段上限却回 400」并不违规（9110 §5.4 只要求 4xx），但「语法错误回 431」是把两个语义混了。
4. **414 只用于长度，不用于字符合法性。** 非法字符（空白、控制字符、原始非 ASCII、`#`）的处置是 400 或 301；301 只在「能把 target 正确编码后重定向」时才成立，对守护进程 API（无浏览器重定向语义）等于只有 400。
5. **204 禁发 Content-Length（MUST NOT）**；304 可发但值必须等于同请求 200 的字节数；HEAD 可发但值必须等于 GET 的字节数。这三条与「HEAD/204/304 不带 body」是两套规则（9112 §6.3 第 1 条管 framing，9110 §8.6 管头部字段），实现上要分开守。
6. **HTTP/1.0 的默认是关连接**，只有请求带 `Connection: keep-alive` 且服务端愿意才持久；若 svcd 决定不对 1.0 持久，则 MUST 在**每个非 1xx 响应**里发 `Connection: close`。pipelining 仍是 MAY，RFC 9112 没有废弃它——「不支持 pipelining」是自由选择，但「按序回响应」是对愿意支持者的硬约束。
7. **一个旋钮的实现里，请求行上限只能通过「整头上限」间接表达**：这与 node/hyper/Go 一致（16 KiB / ~400 KiB / 1 MiB）。若 #251 决定保持单旋钮，那结论必须写成「请求行超长与整头超长统一回 431」，并显式承认这偏离 9112 §3 对 target 超长的 MUST 414。
