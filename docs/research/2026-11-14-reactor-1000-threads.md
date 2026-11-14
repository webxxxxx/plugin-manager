# 20 个 SPK × 1000 客户端线程：reactor 资源回归

> 日期：2026-11-14
> 环境：WSL2 Debian，release 构建
> 脚本：`tools/perf/reactor_scale_threads.py`
> 目的：用 **1000 个真实 OS 客户端线程**复测 HTTP / SSE / WS / MCP，并记录 daemon 的 CPU、RSS/VM、线程数和进程树。

## 方法

- 20 个最简 shell `.spk`，每个有 `echo` / `emit` 能力和 Web UI；
- 1000 个客户端线程，均分到 20 个插件：**每插件 50 并发**；
- 每一轮：
  - HTTP：`GET /plugin-ui/pN/`；
  - MCP：`POST /mcp`，`tools/call pN.echo`；
  - SSE：`/plugin-ui/pN/events`，保持 2 秒；
  - WS：`/plugin-ui/pN/ws`，握手后保持 2 秒；
- 后台每 50 ms 采样：
  - `/proc/<daemon>/status` 的 `VmRSS`、`VmSize`、`Threads`；
  - `/proc/<daemon>/stat` 的 utime/stime；
  - daemon 的子孙进程数（`children` 递归）。

## 结果

| 面 | 请求/连接 | 成功 | 失败 | 耗时 |
|---|---:|---:|---:|---:|
| HTTP | 1000 | 1000 | 0 | 0.605 s |
| MCP | 1000 | 1000 | 0 | 6.314 s |
| SSE | 1000 | 1000 | 0 | 2.482 s |
| WS | 1000 | 1000 | 0 | 2.521 s |

### daemon 资源

| 阶段 | CPU 平均 | CPU 峰值 | 峰值 RSS | 峰值 VM | 峰值线程 | 峰值进程 |
|---|---:|---:|---:|---:|---:|---:|
| HTTP | 29.7% | 35.7% | 3,812 KB（约 3.7 MiB） | 595,976 KB（约 582 MiB） | 10 | 21 |
| MCP | 200.8% | 250.7% | 4,360 KB（约 4.3 MiB） | 596,276 KB（约 582 MiB） | 10 | 24 |
| SSE | 18.9% | 93.4% | 26,300 KB（约 25.7 MiB） | 596,572 KB（约 582 MiB） | 10 | 21 |
| WS | 19.8% | 81.6% | 26,480 KB（约 25.9 MiB） | 596,572 KB（约 582 MiB） | 10 | 21 |

说明：

- **CPU 百分比可能超过 100%**：这是 daemon 进程内多个线程在多核上的累计占用，不是单核上限；不包含插件子进程的 CPU。
- **进程数 21** = daemon 1 个 + 20 个 shell 插件进程；MCP 阶段瞬时 24 个，因为 shell agent 调用了 `sed` 等短命子进程。
- **线程数始终 10**：daemon 主线程、插件 reactor、管理 reactor、HTTP reactor、4 个 HTTP worker、管理客户端 reactor、temp cleanup 线程。
- SSE/WS 阶段 RSS 升到约 26 MB，来自 1000 条连接状态和缓冲；线程数没有随连接数增长。

## 与上一轮对比

上一轮是 20 插件 × 100 并发 = 2000 个 asyncio 连接，daemon 线程峰值也是 10。  
这一轮把客户端换成 **1000 个真实 OS 线程**，daemon 仍是 10 线程，说明连接负载没有把服务端线程模型拉回 thread-per-connection。

## 注意

- 这是 20 个最简 shell 插件的合成基准，不是生产容量承诺。
- 1000 个客户端线程是压测客户端资源，不是 daemon 线程；报告已单独列出两端。
- 插件 `MAX_SUBSCRIBERS = 64`；本轮每插件 50 个 SSE/WS，所以全部成功；上一轮每插件 100 个时触发 64 上限，出现 429。
- Windows 仍是 thread fallback，本结果只代表 Unix/WSL。

## 复现

```bash
cargo build --release
python3 tools/perf/reactor_scale_threads.py \
  --binary target/release/svcd \
  --work /tmp/svcd-reactor-1000 \
  --plugins 20 --threads 1000
```

输出 JSON，并写入 `<work>/results.json`。
