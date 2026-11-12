# 核心能力兼容性与性能测试（HTTP / MCP / WebSocket / SSE）

> 日期：2026-11-13
> 票：[#77](https://github.com/webxxxxx/plugin-manager/issues/77)
> 脚本：`tools/perf/run.mjs`（Node，非 CI）
> 原始数据：`target/perf-results.json` 与 `target/perf-report.md`（target 不入库）

## 环境

| 项 | 值 |
| --- | --- |
| OS | Windows 11（10.0.26200），x86_64 |
| CPU | 12th Gen Intel Core i7-1260P，12 核 / 16 逻辑处理器 |
| 内存 | 16 GiB |
| 构建 | `cargo build --release`，svcd `1,132,544 B` |
| 客户端 | Node v20.11.1；`ws` 8.23.0；`eventsource` 2.0.2；MCP SDK 1.32.2 |
| 插件 | Python 3.10 NDJSON echo agent（含 Python 编解码/调度开销） |
| 网络 | 127.0.0.1，HTTP 无 TLS，端口由 daemon 动态分配 |

## 方法

`node tools/perf/run.mjs --all` 会：

1. 生成临时 config/data/logs/trust，写入测试 token，启动 release `svcd`。
2. 用 `plugin bundle` 打一个最小 echo 插件（带 web UI），通过 HTTP API 安装并启动。
3. 用标准客户端逐项验证 HTTP、MCP、WebSocket、SSE。
4. 用并发 worker 测 HTTP/MCP 的 RPS 与 p50/p95/p99；用长连接测 WS；用 EventSource 测 SSE。
5. 用 4 个 HTTP worker + 4 个 MCP worker + 2 个 WS worker + 1 个 SSE 订阅做 10s 混合负载，并采样 daemon/plugin 的 CPU/RSS。

所有数字是本机单次 release 运行；不是生产压测，也未覆盖 TLS、真实网络、多机或长时间 soak。

## 兼容性结果：18/18 通过

| 检查 | 结果 | 观察 |
| --- | --- | --- |
| HTTP `GET /api/v1/services` | PASS | 200、JSON、Content-Length 匹配、`Connection: close` |
| HTTP 405 | PASS | 带 `Allow: GET` 与 JSON error |
| HTTP 畸形请求 | PASS | 400 |
| HTTP `Transfer-Encoding: chunked` 请求体 | PASS | 4xx 拒绝 |
| MCP `server/discover` | PASS | `2026-07-28`、`resultType: complete` |
| MCP `tools/list` | PASS | 含 `perf.echo` |
| MCP `tools/call` | PASS | `isError: false` |
| MCP 缺 Bearer / 错 Bearer | PASS | 401 / 403 |
| MCP 缺 `_meta` | PASS | 400 + `-32602` |
| MCP **官方 SDK 1.32.2 stdio** | PASS | initialize / list / call 全通（legacy 桥） |
| WS `tools/call` + 通知 | PASS | 响应与 `notifications/message` 同连接 |
| WS ping/pong + close | PASS | 正常 |
| WS 分片文本 | PASS | 服务端重组后处理 |
| WS 跨源 | PASS | 403 |
| WS >1 MiB | PASS | close 1009 |
| SSE EventSource | PASS | 收到通知，带 `id` |
| SSE `Last-Event-ID` | PASS | 重连补发缺失事件 |

### 兼容性边界

- `/mcp` 是 **modern-only（2026-07-28）**；官方 SDK 1.32.2 的 Streamable HTTP 客户端面向较早的 session/SSE 语义，未作为 HTTP 客户端通过；它的 **stdio legacy 桥**已验证通过。HTTP modern 面用脚本内的协议客户端逐项验证。
- HTTP 明确拒绝 chunked 请求体、每个响应 `Connection: close`；不支持 keep-alive、TLS、HTTP/2。
- WebSocket 只支持文本帧、无压缩、无子协议；二进制帧 close 1003，超大消息 close 1009。
- SSE 是 server→UI；上行仍走 `POST /rpc`。

## 分项性能

### 启动与安装

| 指标 | ms |
| --- | ---: |
| daemon 到 HTTP ready | 33.3 |
| plugin bundle | 18.1 |
| plugin install | 34.4 |
| plugin start | 52.7 |

### HTTP（管理 API，每请求新连接）

| 并发 | RPS | p50 ms | p95 ms | p99 ms | 错误 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 47.9 | 20.9 | 21.7 | 22.8 | 0 |
| 8 | 224.3 | 23.3 | 82.3 | 118.0 | 0 |
| 32 | 895.3 | 27.1 | 81.4 | 120.8 | 0 |

### MCP tools/list（Streamable HTTP）

| 并发 | RPS | p50 ms | p95 ms | p99 ms | 错误 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 47.8 | 20.9 | 21.7 | 22.5 | 0 |
| 8 | 214.0 | 23.4 | 84.8 | 125.8 | 0 |
| 32 | 919.7 | 27.1 | 68.2 | 92.7 | 0 |

### MCP tools/call（Streamable HTTP，Python 插件）

| 并发 | RPS | p50 ms | p95 ms | p99 ms | 错误 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 14.4 | 65.0 | 81.1 | 82.3 | 0 |
| 8 | 181.6 | 41.8 | 86.0 | 126.2 | 0 |
| 32 | 875.7 | 27.9 | 73.8 | 123.2 | 0 |

### WebSocket（单连接 vs 多连接）

| 连接数 | RPS | p50 ms | p95 ms | p99 ms | 错误 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 15.7 | 64.2 | 65.1 | 65.5 | 0 |
| 8 | 819.7 | 1.5 | 21.3 | 63.7 | 0 |
| 32 | 1296.6 | 13.8 | 46.9 | 66.1 | 0 |

### SSE（逐次 `POST /rpc` 触发，300 个事件）

| 事件 | 总数 | 事件/秒 | 丢失 |
| ---: | ---: | ---: | ---: |
| 300 | 300 | 14.6 | 0 |

### 混合负载（10s）

| 指标 | 值 |
| --- | ---: |
| HTTP 操作 | 1099 |
| MCP 操作 | 1131 |
| WS 操作 | 3848 |
| SSE 事件 | 3848 |
| 总 RPS | 607.8 |
| 错误 | 0 |

### 资源

| 进程 | CPU 前后（秒） | 工作集前后（B） |
| --- | ---: | ---: |
| daemon | 0.00 → 34.69 | 6,377,472 → 8,679,424 |
| plugin | 0.00 → 0.00（Python 子进程未单独计入） | 7,675,904 → 7,630,848 |

daemon 在 10s 混合负载内消耗约 34.7 秒 CPU（约 3.5 个逻辑核，峰值的 22% 左右），RSS 峰值约 8.7 MiB；没有错误。

## 结论与瓶颈

1. **单连接 HTTP 延迟被 20ms 的 accept 轮询限制。** `src/api.rs` 的 listener loop 在每次 accept 尝试后 `sleep(20ms)`，单请求 p50 稳定在约 20.9ms，单连接只有约 48 RPS。并发高时内核 backlog 暂时吸收连接，RPS 能到约 900，但 p95 上升到 80ms 左右。
2. **插件调用的 50ms 事件循环进一步抬高延迟。** `src/daemon.rs` 的 event loop 用 `recv_timeout(50ms)`；MCP tools/call、WS tools/call、SSE 触发都经过它，单次 p50 约 65ms（20ms accept + 最多 50ms 轮询）。并发高时 WS 因为长连接不复用 accept 成本，8 连接就能到约 820 RPS。
3. **SSE 的 14.6 事件/秒是逐次 POST 造成的，不是 SSE 本身。** 每个事件都要走一次新的 HTTP 连接 + 50ms daemon 轮询；持续推送场景应使用插件的定时通知，而不是每次请求触发。
4. **MCP modern 协议面工作正常；官方 SDK 的 HTTP 客户端时代不匹配，stdio legacy 兼容良好。**
5. **资源占用很小**：daemon RSS 约 8.7 MiB，Python 插件约 7.6 MiB；二进制 1.13 MB（Windows）/1.36 MB（musl），仍在门禁内。

## 建议

- **P0**：把 `src/api.rs` 的 accept 循环从「非阻塞 accept + 20ms sleep」改成阻塞 accept + rebind 唤醒通道（或 poll），单连接 HTTP/MCP 延迟目标 < 2ms。
- **P0**：把 `src/daemon.rs` 的 50ms 轮询改成更短 tick 或 condvar/eventfd；插件调用 p50 目标 < 5ms。
- **P1**：为 HTTP 管理 API 与 `/mcp` 加 keep-alive；当前每请求 TCP 建连会放大延迟。
- **P1**：性能基线使用 native 插件（Rust/C）+ mock 上游，把 Python 开销与核心开销分开；本次数字已够做相对比较，不适合作为绝对上限。
- **P2**：性能脚本保留为非 CI 工具；如果要防回归，另建 nightly 或专门 runner，不要塞进普通 `cargo test`。

## 复现

```sh
cd tools/perf
npm install
cd ../..
cargo build --release
node tools/perf/run.mjs --all
```

报告与 JSON 写到 `target/perf-report.md`、`target/perf-results.json`。
