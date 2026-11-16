# 20 插件 × 50 WebSocket：512 KiB JSON echo 连续 60 秒

日期：2026-11-16  
平台：WSL2 Debian Linux x86_64  
构建：`cargo build --release`，默认 `MEM_LEVEL=high`  
连接：20 个 WebUI 插件，每个 50 条 WebSocket，共 **1000 条**  
消息：每个连接连续发送 512 KiB JSON-RPC `tools/call`，收到 echo 后立即发下一条

## 配置

```text
max_connections = 4096
max_connections_per_ip = 4096
max_streams = 4096
max_subscriptions = 2048
max_subscribers_per_plugin = 64
http_idle_timeout_secs = 300
```

每个插件的 `echo` capability 返回：

```json
{"jsonrpc":"2.0","id":1,"result":{"data":"<原样返回的512 KiB数据>"}}
```

客户端 1000 条连接各自独立循环，不做轮次屏障：收到响应后马上发下一条。

## 结果

```text
plugins                 20
per_plugin_ws           50
total_ws                1000
payload_bytes           524288

requests_completed      6131
data_ok                 6131
errors                  0
ws_open                 1000

handshake_seconds       21.58
transfer_seconds        70.27   # 60 秒发送窗口 + 等待在途响应
send_window_seconds     60

payload_sent_mb         3065.50
bytes_sent              3215089519
bytes_received          3214762619

throughput_60s_mb_s     51.09
throughput_60s_mbit_s   408.72
throughput_total_mb_s   43.62
throughput_total_mbit_s 365.94

latency_ms_min          1419.87
latency_ms_avg          10615.63
latency_ms_max          12425.83

daemonRssAvgMB          1538.20
daemonRssMaxMB          1594.04
pluginRssAvgMB          255.83
pluginRssMaxMB          261.34
totalRssAvgMB           1794.04

daemonThreadsMax        10
daemonCpuPercent        322.32    # 所有核合计，约 3.22 核
pluginCpuPercent        38.18
daemon_fatal_markers    []
data_log_lines          71
```

## 日志检查

`data.log` 每秒一行，共 71 行（60 秒窗口 + 约 10 秒在途排空）：

```text
# plugins=20 per_plugin=50 connections=1000 payload=524288 bytes continuous=60s
t=01 done=0    open=1000 sent_mb=499.1  recv_mb=0.0    errors=0
t=02 done=35   open=1000 sent_mb=516.9  recv_mb=21.0   errors=0
...
t=66 done=5727 open=1000 sent_mb=3066.1 recv_mb=2864.0 errors=0
t=67 done=5824 open=1000 sent_mb=3066.1 recv_mb=2912.9 errors=0
t=68 done=5926 open=1000 sent_mb=3066.1 recv_mb=2963.9 errors=0
t=69 done=6015 open=1000 sent_mb=3066.1 recv_mb=3007.8 errors=0
t=70 done=6107 open=1000 sent_mb=3066.1 recv_mb=3053.8 errors=0
```

检查结果：

- 6131 条请求全部完成；
- `data_ok = 6131`，所有返回 `data` 都与发送的 512 KiB 数据完全一致；
- 客户端错误数 **0**；
- 每条日志都是 `open=1000`、`errors=0`；
- 结束时 1000 条 WS 仍全部打开；
- daemon stderr / `svcd.log` 没有 `panic`、`overflow`、`fatal`、`abort`、`too large`、`reset`。

## 解读

- 60 秒窗口内完成 **6131 条** 512 KiB echo，合计 **3065.5 MiB** payload；
  按 60 秒窗口计吞吐约 **51.09 MB/s（408.7 Mbit/s）**。
- 如果按实际等待在途响应完成的 70.27 秒计，吞吐约 **43.62 MB/s（365.9 Mbit/s）**。
- 端到端延迟：
  - 最小 **1.42 秒**
  - 平均 **10.62 秒**
  - 最大 **12.43 秒**
- daemon RSS 约 **1.54 GB**，峰值 **1.59 GB**；总 RSS 约 **1.79 GB**。
  这是 1000 条连接同时持有 512 KiB 请求/响应和 echo 队列的成本，不适合 50 MB 设备。
- daemon 线程数仍为 **10**，没有随 1000 条连接增长。
- daemon CPU 约 **3.22 个核**，20 个 Python echo 插件合计约 **38%**。
- 连续发送不会出现“某一轮等最慢连接”的空档；每个连接收到自己的 echo 后立即开始下一次。

## 复现

脚本：`target/bench/run_ws_echo_continuous.py`（本地基准脚本，不提交到仓库）  
记录：`/tmp/svcd-echo/run/summary.json`、`/tmp/svcd-echo/run/data.log`
