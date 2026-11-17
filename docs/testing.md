# 测试策略

> 架构分层见 [architecture.md](architecture.md)。本文只回答：什么测试放在哪一层、跑在什么时候、失败代表什么。

## 1. 先纠正一个术语

我们常说的「W3C 兼容性」实际横跨四个不同标准组织，不能用一个 W3C 测试套件覆盖：

| 面 | 真正的标准 | 说明 |
|---|---|---|
| HTTP/1.1 | IETF RFC 9110（语义）/ RFC 9112（报文） | W3C 不定义 HTTP/1.1 线格式；WPT 是浏览器测试，不是服务器一致性套件 |
| WebSocket | IETF RFC 6455 | 事实标准套件是 Autobahn TestSuite，不是 W3C |
| SSE | WHATWG HTML Living Standard §9.2 | 不是 W3C；WPT 的 `eventsource` 测试是浏览器级 |
| MCP | Model Context Protocol 规范 | 不是 W3C；官方 SDK 是最接近一致性客户端的工具 |

所以目标不是「跑一个 W3C 套件」，而是**按各自规范选对套件/向量，并测到线格式和错误路径**。

## 2. 四层测试

| 层 | 内容 | 什么时候跑 | 工具 |
|---|---|---|---|
| L1 单元 / 契约 | 解析、上限、状态机、失败类别、配置夹取 | 每个 PR | `cargo test` |
| L2 协议兼容 | HTTP/1.1、WebSocket、SSE、MCP 的 framing、handshake、错误码、重连 | 每个 PR（轻量）+ 每晚（完整） | `tools/perf/run.mjs --compat`、Autobahn、官方 SDK |
| L3 性能 | 延迟分位、吞吐、RSS、线程、插件规模、混合负载 | 每晚 / 手动 | `tools/perf/run.mjs --perf`、`--scale`、soak 脚本 |
| L4 资源档位 | `MEM_LEVEL=low/middle/high` 的默认值与边界 | 每个 PR（矩阵） | `cargo test --bin svcd limits::tests` |

原则：**L1 必须快且确定；L2 测线格式而不是业务；L3 不进 PR 门槛；L4 必须覆盖三档编译。**

## 3. 协议兼容性：测什么

### 3.1 HTTP/1.1（RFC 9112）

已有 `tests/http.rs` 覆盖 chunked、`Expect: 100-continue`、Range、keep-alive 等；还缺一组「原始 socket 向量」：

- 请求行 / header 的非法字符、obs-fold、重复 `Content-Length`、`Transfer-Encoding` 与 `Content-Length` 冲突；
- `HEAD` / `204` / `304` 不得带 body；
- `413` / `414` / `431` 的边界与 `Connection: close`；
- HTTP/1.0 与 `Connection: keep-alive`、pipelining 的响应顺序。

**不引入 h2spec**：当前实现是 HTTP/1.1，没有 HTTP/2；加 HTTP/2 时再引入。

### 3.2 WebSocket（RFC 6455）

- 握手：`Upgrade`、`Connection`、`Sec-WebSocket-Version: 13`、`Sec-WebSocket-Key`、`Sec-WebSocket-Accept`；
- 帧：分片、控制帧、ping/pong、close code、掩码、超长消息 1009；
- **Autobahn TestSuite** 作为完整一致性套件（Docker `crossbario/autobahn-testsuite`），只跑 `wstest` 的 fuzzingclient；
- 对 `/plugin-ui/.../ws` 与原始 WS 通道分别跑，不能只测一个端点。

### 3.3 SSE（WHATWG HTML §9.2）

- 字段：`event:`、`data:`（多行拼接）、`id:`、`retry:`、注释行 `:`；
- 换行：LF / CRLF / CR 都要接受；BOM 只在流开头允许；
- 重连：`Last-Event-ID` 回放；无 `id` 的事件不改变 last id；
- 非法 UTF-8 替换为 U+FFFD，不中断流。

现有 `run.mjs --compat` 只测 EventSource 投递与 `Last-Event-ID`，上面这些要补成表驱动向量。

### 3.4 MCP

- 官方 `@modelcontextprotocol/sdk` 客户端：`initialize` / `server/discover` / `tools/list` / `tools/call`；
- `MCP-Protocol-Version`、`_meta`、Bearer 401/403、缺失参数 `-32602`；
- progress notification 与 SSE 流式回复；
- 双时代 stdio bridge 与 Streamable HTTP 各跑一遍。

MCP 不是 W3C，不要用 WPT 套；官方 SDK + 规范逐条向量就是这一层的「一致性测试」。

## 4. MEM_LEVEL 与 `svcd.conf` 边界

`MEM_LEVEL=low|middle|high` 是编译期档位，`svcd.conf` 是运行期覆盖；两者都在 [limits.rs](../src/limits.rs) 夹取。测试分三段：

1. **档位矩阵**：CI 用 `low/middle/high` 各编译并跑 `limits::tests`。
2. **边界夹取**：每个字段取 `0` / `1` / `MAX`，断言结果落在文档下限/上限，且不 panic。
3. **默认值**：当前档位的默认值必须落在同一组下限/上限内；`default_timeout <= max_timeout`。

已覆盖的坑：`max_streams=0`、`request_stack_bytes=1`、`io_stack_bytes=1`、`idle_timeout=0`、`u64::MAX` 超时。

第 4 条已经落地：`tests/http.rs::minimal_svcd_conf_keeps_the_daemon_alive` 用两份 `svcd.conf` 驱动真实 daemon，比纯 `Limits` 单测更接近用户报的「设置过小崩溃」。

- **floor**：每个键都取解析器接受的最小值（0/1）。断言夹取真的到了运行期（超长 header 400、超 body 413、零订阅 429、超连接 503），进程不退出、不 panic、不重启。
- **usable**：同样的下限，只把「0 等于关掉功能」的键抬到最小可用值（连接 2、订阅 1、`default_timeout` 1000 ms），让 HTTP / MCP / WebSocket / SSE 各完成一次真实往返：WS ping/pong 与 1009、SSE 投递与 `Last-Event-ID` 重连、`/mcp` 的 `tools/list`/`tools/call` 与 stdio bridge 的 `initialize`。

每条探针都归属一个 `svcd.conf` 键，失败时打印该键、原始请求、响应和 daemon 输出。

Windows 走每连接线程回退，但上面的连接数、每 IP 与 idle 探针在两端断言同一结果：回退在 accept 后、起线程前准入，线程数上限由 `max_connections` 约束，不是最终的四线程拓扑。

## 5. 性能测试

`tools/perf` 已经是完整的兼容 + 性能 harness，但**不在 CI**：

- `--compat`：协议/客户端兼容；
- `--perf`：HTTP / MCP / WS / SSE 在并发 1/8/32 下的延迟与吞吐；
- `--scale`：0/5/10/30 插件的进程树 RSS/CPU；
- `reactor_scale*.py`：20/1000 连接与 soak。

建议的 CI 策略：

- **PR**：不跑真实网络性能（噪声大），只保留 `spk_smoke.py`；
- **每晚 / 手动**：跑 `--perf`，上传 `target/perf-report.md` 与 JSON；
- **基线**：先记录同一 runner 的 p50/p95/p99、RSS、线程数；稳定后对 p95 与 RSS 设 20% 回归阈值，超过才失败；
- **MEM_LEVEL**：对 low/middle/high 各跑一次空载 RSS，确认低档确实更低，且没有 OOM/重启。

性能数字是机器相关的；不要拿不同 runner 的绝对值当门槛，只做同机相对比较。

## 6. 命令

```sh
# L1 + L4（摘要输出：每个 target 一行，失败才打印尾部）
node tools/check.mjs quick
node tools/check.mjs test <filter>
node tools/check.mjs full

# 原始 cargo（需要完整输出时）
cargo test
MEM_LEVEL=low cargo test --bin svcd limits::tests -- --test-threads=1
MEM_LEVEL=middle cargo test --bin svcd limits::tests -- --test-threads=1
MEM_LEVEL=high cargo test --bin svcd limits::tests -- --test-threads=1

# 运行时崩溃回归（#108，MEM_LEVEL 各跑一次）
cargo test --test http minimal_svcd_conf -- --test-threads=1
MEM_LEVEL=low CARGO_TARGET_DIR=target/mem-low cargo test --test http minimal_svcd_conf -- --test-threads=1

# L2（需要 release 二进制 + tools/perf 依赖）
cargo build --release
cd tools/perf && npm ci && cd ../..
node tools/perf/run.mjs --compat

# L3
node tools/perf/run.mjs --perf
node tools/perf/run.mjs --scale
```

### 本机并行测试的环境约束

集成测试的 sandbox 根目录在 Windows 使用 Cargo 提供的 `CARGO_TARGET_TMPDIR`（在 `target/tmp`
下），不再用系统 `%TEMP%`：受限的 TEMP ACL 曾让 `create sandbox` 直接 `PermissionDenied`，target
目录始终可写。Unix 保持系统临时目录，因为 Unix domain socket 对整条路径有 ~108 字节上限，target 下
的深路径会直接 `cannot bind control channel`。单测写入临时目录的地方统一走 `src/test_support.rs::temp_dir()`（Windows 下是 `target/<profile>/tmp`，Unix 仍是系统 temp）。`tests/cli.rs` 对并发的 `ETXTBSY`（复制 multicall 二进制后立即 exec）做有限重试；不要把它
当成测试失败。运行时记录的单测通过 `runtime::tests::with_root` 注入 root，**不要**再在测试里设
`SVCD_RUN_DIR`（进程级 env + 全局缓存会让并行测试互相踩）。CI 仍用 `--test-threads=1`，本地并行
跑全量时若看到这些类别失败，先按本节判断是不是环境噪声。

### 异步等待与失败诊断

- `tests/http.rs` 的 `wait_until` 在超时时会打印 daemon log 和调用方提供的诊断（例如 stalled socket 上已到达的响应）；新增等待 helper 必须走它，不要写裸 `sleep` + `panic!`。
- 不要在转瞬即逝的中间状态上做轮询断言（例如 staging 刚创建就因 `already installed` 被删除）；先让触发路径确定（加 `force=true`、补完 body），或直接断言最终的结束状态。

线程预算回归（Linux 补充）：`cargo test --test http unix_idle_thread_count_has_no_per_request_or_per_stream_threads -- --exact`。它读 `/proc/<pid>/status` 的 `Threads:`，断言 #126 后的默认空载基线为 4 条 daemon 线程（supervisor——同时 poll 插件 stdio、server loop、2 条阻塞线程）；16 个 keep-alive 连接与 8 个 SSE 流不得各增加一条线程。改变拓扑的切片要同步更新 `IDLE_DAEMON_THREADS`。同一测试还读 `/proc/<pid>/task/*/comm`，断言 `svcd-channel-reactor` / `svcd-channel-client` 不再存在，`svcd-http-worker` / `svcd-temp-cleanup` / `svcd-import-http` / `svcd-install-http` / `svcd-reactor` 不再存在，`svcd-server-loop` 恰好一条；`control_shutdown_reply_is_flushed_before_exit` 在原始控制 socket 上断言 shutdown 回包在进程退出前到达；`server::control` 单测覆盖两个控制连接的回包路由、满 256 队列的 -32000、queued reply 的 shutdown 排空、缺失控制 socket 时的 SSE -32603、无 HTTP 的 control-only loop；`a_busy_http_port_keeps_the_control_channel_alive` 证明 HTTP bind 失败只停 HTTP、CLI 仍可用。

MCP SSE 的 handler 由 `cargo test --bin svcd server::mcp::progress` 覆盖：SSE 头 / chunk 编码、progress 先于 final、-32603 映射、小输出缓冲的分片、断开时取消恰好一次，以及「输出被堵住时不得延长 no-progress 上限」。Unix 上还有 `server::http::driver` 的 wakeup 测试：短 poll wakeup 只提前 tick，不能替换传输 deadline。集成回归是 `cargo test --test http http_mcp_streams_progress_for_tools_call`（顺序断言）与 `http_mcp_stream_consumes_the_long_lived_max_streams_budget`（`max_streams = 1` 时 503 + `Retry-After`，释放后同一调用得到 200 SSE）；Unix 另有 `http_mcp_stream_disconnect_releases_the_budget`（断开的慢调用立即释放槽；Windows 回退没有可取消的 in-flight call，不断言这一条）。

另外，#118/#119 有传输专用回归：`cargo test --test http unix_blob_download_has_no_takeover_thread -- --exact` 与 `unix_upload_has_no_takeover_thread -- --exact`。它们让慢读者/慢写者挂住传输，断言 daemon 线程数不变——旧 takeover 会多出一条 `svcd-blob-http`。跨平台的 `a_slow_blob_reader_drops_the_stream_cap_and_leaves_the_loop_free`、`a_slow_file_bundle_reader_drops_the_stream_cap_and_leaves_the_loop_free` 与 `a_slow_upload_holds_the_stream_cap_and_leaves_no_staging_file` 同时断言传输只占 `max_streams` 的一个槽（第二个请求得到 503 + `Retry-After: 1`）、其他请求仍能完成，并在断开后释放槽；上传版本还断言 staging 临时文件最终消失。`server::http::driver::tests::a_file_chunk_never_grows_past_the_stream_buffer` 覆盖无 sendfile 时输出缓冲的高水位；`a_blocking_job_pauses_input_until_its_completion` 覆盖上传背压：job 在飞行时 driver 不读下一块。

#125 删除 legacy bridge 后没有独立的 HTTP worker 队列：所有不能在 loop 上跑的路由（MCP、插件 UI rpc、blob 会话元数据）都提交给同一个阻塞池，池队列上限 64，满时 503 `busy` + `Retry-After: 1`。`server::http::pool` 的单测覆盖满队列与每连接一个 job；`tests/http.rs` 的并发/限流用例覆盖实际回包。

#122 的插件 UI 实时流由 `cargo test --bin svcd face::plugin_streams` 的无 socket 单测覆盖：SSE 分片/重放/心跳（含「部分事件在途时 tick 不得覆盖 pending」）、WS 握手/分片/控制帧/1009/1013 与响应先于队列下一条调用、raw 二进制/分片/关闭码。真实 TCP 回归是 `tests/http.rs` 的 `plugin_ui_*` 用例；`plugin_ui_realtime_streams_consume_the_long_lived_stream_cap` 断言 `max_streams = 1` 时第二条 SSE/WS 得到 503 + `Retry-After: 1`、静态资源仍豁免、释放后可重连。页面 WS 的 `tools/call` 走阻塞池（有界 64）而不是 legacy worker 队列；`ws_inflight_requests` / `ws_inflight_bytes` 约束已排队但未提交的调用。

## 7. CI 落点

| 工作流 | 触发 | 内容 |
|---|---|---|
| `test.yml` | PR / main | `cargo test`、`--no-default-features`、`.spk` smoke、`MEM_LEVEL` 三档 |
| `test.yml` guardrails | PR / main | `tools/closed-tickets-merged.mjs --map 112`：已关闭子票必须有提交在 base |
| `perf.yml` | 每周 / 手动 | `--perf`，上传报告与 JSON |
| 兼容套件（待建） | 每晚 / 手动 | `--compat`、Autobahn、WHATWG SSE 向量 |

性能与兼容的完整套件不要塞进 PR：PR 只保留可确定、分钟级的检查。