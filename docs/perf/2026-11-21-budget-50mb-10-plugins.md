# 内存预算 50 MB × 10 插件：五面流量与三形状空载 RSS

日期：2026-11-21  
平台：Windows x86_64  
构建：`cargo build --release`，main `f71f7d7` + 本地 perf 场景脚本  
插件：10 个（Python NDJSON fixture，带 `echo` 能力与 WebUI）

## 方法

- 每次启动一个全新的 `svcd`；`svcd.conf` 只写 `bind`、`mem_budget_bytes`
  和测试用的 `auth_passwd`（MCP 的 Bearer 与它比对），`workload` 保持派生。
- 五面流量：每个插件都在同一轮里被驱动 invoke（MCP `tools/call`）、SSE、
  WebSocket、blob upload、blob download；10 个插件同时存活，总时长 60 秒，
  驱动并发 4。
- 每 2 秒采样 daemon 与进程树 RSS/CPU/线程：Windows 走 psapi + Toolhelp32，
  不启动 PowerShell，避免采样本身干扰。
- 最后一轮超订探针：对 10 个插件各开 1 条 SSE 与 1 条 WS，检查拒绝是否为
  `429` / `503`，然后确认 daemon 仍能完成一次正常 invoke。

## 预算与派生

请求的 `mem_budget_bytes = 52428800`（50 MB）**不能生效**：启动报告给出
`below-floor`，实际预算抬到 **64 MiB**，并向下量化到 64 MiB；形状派生为
`tiny`。

| 请求 | 生效预算 | 形状 | worst_case | 覆盖说明 |
|---:|---:|---|---:|---|
| 50 MB | 64 MiB | tiny | 40.45 MiB | `mem_budget_bytes` 52428800 → 67108864（below-floor） |
| 512 MiB | 512 MiB | interactive | 240.50 MiB | 无 |
| 2 GiB | 2 GiB | streaming | 1899.70 MiB | 无 |

同样的 10 个插件，三档派生出的上限差异很大：

| 形状 | max_connections | per_ip | max_streams | max_subscriptions | max_complete_body |
|---|---:|---:|---:|---:|---:|
| tiny @ 64 MiB | 12 | 8 | 8 | 4 | 32 KiB |
| interactive @ 512 MiB | 128 | 32 | 16 | 16 | 1 MiB |
| streaming @ 2 GiB | 682 | 64 | 64 | 64 | 2 MiB |

## 结果 A：50 MB × 10 插件 × 60 秒 × 五面

| 插件 | invoke | sse | ws | upload | download |
|---|---:|---:|---:|---:|---:|
| p00 | 103/103 | 103/103 | 103/103 | 103/103 | 103/103 |
| p01 | 103/103 | 103/103 | 103/103 | 102/102 | 102/102 |
| p02–p09 | 102/102 | 102/102 | 102/102 | 102/102 | 102/102 |

- 总成功操作 **5108** 次，状态码全部 200；10/10 插件存活到结束。
- daemon RSS **11.01 MB 平均 / 11.61 MB 峰值**（上限 64 MiB）。
- 进程树 RSS 191.97 MB 平均 / 192.27 MB 峰值；约 190 MB 是 10 个
  Python 解释器 + `cmd.exe`，不计入 `B`。
- daemon 线程峰值 35；daemon CPU **42.62%**（单核），进程树 43.21%。

超订探针（10 条 SSE + 10 条 WS 同时开）：

| 面 | 结果 |
|---|---|
| SSE | `200 × 2`，`429 × 8` |
| WS | `101 × 2`，`429 × 8` |
| 恢复 | 探针关闭后一次 invoke 正常（200） |

先到上限的是 **subscriptions = 4**：2 SSE + 2 WS 用满，其余 16 条全部
`429`，没有 5xx，也没有崩溃。

原始记录：`target/budget-50mb-10p-results.json` / `target/budget-50mb-10p-report.md`。

## 结果 B：三形状空载（10 插件 × 60 秒）

| 形状 | daemon RSS 平均 | daemon RSS 峰值 | 进程树 RSS 平均 | 进程树 RSS 峰值 | 线程峰值 | daemon CPU | 存活 |
|---|---:|---:|---:|---:|---:|---:|---:|
| tiny | 9.56 MB | 9.57 MB | 190.45 MB | 190.93 MB | 27 | 0.18% | 10/10 |
| interactive | 9.56 MB | 9.57 MB | 190.80 MB | 191.30 MB | 27 | 0.26% | 10/10 |
| streaming | 9.72 MB | 9.73 MB | 191.02 MB | 191.49 MB | 27 | 0.10% | 10/10 |

原始记录：

- `target/budget-idle-results.json` / `target/budget-idle-report.md`
- `target/budget-interactive-results.json` / `target/budget-interactive-report.md`
- `target/budget-streaming-results.json` / `target/budget-streaming-report.md`

## 解读

- **50 MB 不是一个可表达的预算**：64 MiB 下限先于一切派生生效。要测 50 MB
  设备，必须把下限也当作被测对象，而不是把 50 MB 当输入。
- **形状改的是上限，不是空载占用**：三档 daemon RSS 都在 9.6–9.7 MB，
  差异只有 0.16 MB；真正变的是并发/流/订阅/单体上限（12 → 682 条连接）。
- **五面并发在小预算下能跑，但只能压在预算内**：并发 4 时 60 秒内
  5108 次操作全部成功，daemon 11 MB 远低于 64 MiB；把并发拉到 20 条
  长连接时，先撞 `max_subscriptions = 4`，多余请求得到 `429`，daemon
  不崩、随后仍能正常 invoke。
- **「10 个插件各持 SSE + WS 同时跑」在 50 MB 档装不下**：20 条长连接
  超过 tiny 的 `max_connections = 12` / `max_streams = 8` /
  `max_subscriptions = 4`。要真的同时持住，得用 streaming 形状
  （64 流 / 64 订阅）或显式抬高上限；否则测试应把并发压在预算内，或把
  429/503 当作预期结果。
- **五面流量的 CPU 在 42% 单核**：主要来自 4 个驱动线程 + Python fixture
  的 JSON-RPC 解析，不代表 svcd 的协议开销。

## 复现

```sh
cargo build --release

# 50 MB 请求 → 64 MiB / tiny；10 插件 × 60 秒 × 五面
python tools/perf/budget_scenario.py \
  --budget 50mb --plugins 10 --duration 60 --concurrency 4

# 三档空载基线
python tools/perf/budget_scenario.py --traffic idle \
  --budget 50mb --plugins 10 --duration 60
python tools/perf/budget_scenario.py --traffic idle \
  --budget 512mb --plugins 10 --duration 60
python tools/perf/budget_scenario.py --traffic idle \
  --budget 2gb --plugins 10 --duration 60
```

`--bootstrap api`（默认）走 `POST /api/v1/plugins` 的签名安装路径；
`--bootstrap direct` 只用于控制通道不可用的测量主机，会跳过签名校验与暂存。

## 备注

- 同一轮排查发现 `tools/perf/run.mjs` 仍在 `<config>/http.port` 找地址；
  ADR-0023 之后端口文件在 `<data>/run/http.port`。已让 `run.mjs` 兼容两处。
- 本机最初是 Low integrity 会话，daemon 的控制命名管道对同进程 HTTP 面也
  返回 `os error 5`，install/invoke/status 全部不可用；切到普通权限后同一份
  脚本无需改动即跑通五面。这说明该失败是宿主会话限制，不是产品缺陷。
