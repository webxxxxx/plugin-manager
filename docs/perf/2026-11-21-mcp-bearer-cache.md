# MCP Bearer 缓存：每个请求 11.3 ms → 0.9 ms（2026-11-21）

追 #107 的「MCP 请求比管理 API 慢 10 倍」时量出来的固定成本。它不在控制通道、
不在事件总线、不在插件，而是 **PBKDF2 在每个 MCP 请求上重算了一遍 Bearer**。

## 分段测量

同一个守护进程（10 个 bash shell 插件，五面空闲），同一时刻顺序发 100 次请求，
每个请求一条新连接：

| 路径 | 修前 p50 | 修后 p50 |
| --- | ---: | ---: |
| `GET /api/v1/services`（服务端自有路由，无控制调用） | 0.91 ms | 0.94 ms |
| `POST /mcp` `tools/list` | 11.29 ms | **0.89 ms** |
| `POST /mcp` `tools/call`（p00） | 11.30 ms | **0.89 ms** |
| `POST /mcp` `tools/call`（p01） | 11.26 ms | **0.89 ms** |
| UDS `svcd.ping`（控制往返） | 0.07 ms | 0.09 ms |
| UDS `registry.tools`（控制往返） | 0.11 ms | 0.13 ms |

三个 MCP 请求、两种不同方法、两个不同插件，全部落在 11.3 ms：说明是**每请求固定成本**，
不是某个方法或某个插件的开销。同一时刻的控制往返只有 0.07–0.13 ms，所以 10.4 ms 的差额
不在通道上。

## 原因

`src/server/mcp/transport.rs::authorize` 调的是 `platform::auth::verify_password`——每次都跑
PBKDF2-SHA256（10000 轮），在 WSL 上约 10 ms。而 `VerifyCache` 的文档注释写的正是这件事：

> Bearer verification runs on every machine request and re-deriving the KDF there costs
> more CPU than the request itself.

HTTP 管理 API（`server/auth/lan.rs:134`）早就走 `verify_password_cached`，只有 `/mcp` 漏了。

## 改动

```rust
// src/server/mcp/transport.rs
Some(hash) if crate::platform::auth::verify_password_cached(bearer, hash) => Ok(()),
```

缓存的契约（`src/platform/auth.rs`）：

- 键是 `SHA-256(存储的 hash, 提交的口令)`，只存摘要，**不存口令**；
- 只记一次成功的验证；失败仍然每次付满 KDF，随机口令打不中缓存；
- TTL 300 s、上限 32 条，命中一次是一次 SHA-256；
- 换 `auth_passwd` → 存储的 hash 变 → 键变，旧条目不可能放行新配置。

## 注入测试

把调用改回 `verify_password`，新加的
`server::mcp::transport::tests::an_authorized_request_leaves_the_bearer_in_the_cache`
立刻红（0.16 s 内失败）；改回 `verify_password_cached` 后绿。这条测试断言的是「授权一个请求后，
这个凭证必须已经进了缓存」，不靠计时，所以在快机器上也不会飘。

## 吞吐

WSL2 / 50 MB / 10 插件 / 五面全开 / 30 s，与 `2026-11-21-throughput-after.md` 同参数：

| cell | 修前 | 修后 | 变化 |
| --- | ---: | ---: | ---: |
| 50mb-c1 | 238.5 ops/s，最慢面 p50 47 ms | **429.5 ops/s**，最慢面 p50 5.0 ms | +80% |
| 50mb-c10 | 327.1 ops/s，最慢面 p50 422 ms | **1329.5 ops/s**，最慢面 p50 72.6 ms | +306% |

两格可用率都是 100%（12890/12890、39951/39951）。c10 的 4 倍是因为 100 个 worker 之前
在排队等同一把 KDF：daemon 的 CPU 全烧在派生密钥上，真正干活的时间被挤掉了。

## 容量边界（12 s/格，10 插件，五面全开，50 MB）

| 每插件并发 | 可用率 | ops/s | 最慢面 p50 |
| --- | ---: | ---: | ---: |
| c12 | 100%（16502/16502） | 1369.5 | 90.7 ms |
| c16 | 100%（15686/15686） | 1296.5 | 134.5 ms |
| c20 | 99.92%（12 × 503） | 1258.6 | 174.7 ms |
| c24 | 99.72%（43 × 503） | 1259.0 | 203.9 ms |
| c28 | 99.49%（80 × 503） | 1294.3 | 227.9 ms |

满可用边界从 c12（302.9 ops/s）挪到 c16（1296.5 ops/s）：4.3 倍。c20 之后开始出 503，是
288 条连接的不变量在挡（c28 就是 280 个并发 worker），不是 CPU，也不是 KDF。

## 因此没有做的两项

原计划里的「进程内 `RegistryCall` seam」与「池线程直调」是冲着那 11 ms 去的。分段测量说明
这 11 ms 不在通道上，控制往返只有 0.07–0.13 ms——占一个 0.89 ms 请求的 11%。把 face 层
20 个 `channel::request` 调用点全部换签名，只换来约 0.1 ms，收益与风险不成比例。
若以后控制通道本身变贵（例如跨进程拆分），这条 seam 再议。
