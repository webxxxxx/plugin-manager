# 20 个 SPK × 1000 客户端线程 × 3 分钟 soak

> 日期：2026-11-14
> 环境：WSL2 Debian，release 构建
> 脚本：`tools/perf/reactor_scale_soak.py`
> 目的：在 3 分钟持续负载下观察 reactor 的 HTTP / SSE / WS / MCP 稳定性，以及 daemon 的 CPU、RSS/VM、线程数、进程数。

## 方法

- 20 个最简 shell `.spk`，每个 `echo` / `emit` 能力 + Web UI；
- 1000 个真实 OS 客户端线程，均分到 20 个插件：**每插件 50 并发**；
- 循环执行四段：
  - HTTP：1000 个 `GET /plugin-ui/pN/`；
  - MCP：1000 个 `POST /mcp`，`tools/call pN.echo`；
  - SSE：1000 条 `/plugin-ui/pN/events`，保持 2 秒；
  - WS：1000 条 `/plugin-ui/pN/ws`，握手后保持 2 秒；
- 持续 **180 秒**；实际完成 16 个完整循环，耗时 **181.324 秒**；
- HTTP/MCP 使用每线程持久连接，避免短时间反复建连把客户端临时端口耗尽；SSE/WS 每轮新建 1000 条连接并在 2 秒后关闭；
- 每 50 ms 采样 daemon：
  - `/proc/<pid>/status` 的 `VmRSS`、`VmSize`、`Threads`；
  - `/proc/<pid>/stat` 的 utime/stime；
  - 子孙进程数。

## 结果

共 16 个完整 cycle，每个面 **16,000** 次操作：

| 面 | 尝试 | 成功 | 失败 | 累计耗时 |
|---|---:|---:|---:|---:|
| HTTP | 16,000 | 16,000 | 0 | 3.58 s |
| MCP | 16,000 | 16,000 | 0 | 100.07 s |
| SSE | 16,000 | 16,000 | 0 | 38.48 s |
| WS | 16,000 | 16,000 | 0 | 38.95 s |

**总计 64,000 次操作，0 失败。**

### daemon 资源（181.324 秒）

| 指标 | 值 |
|---|---:|
| CPU 平均 | **125.9%** |
| CPU 采样峰值 | **245.5%** |
| 峰值 RSS | **31,224 KB（约 30.5 MiB）** |
| 峰值 VM | **597,168 KB（约 583 MiB）** |
| 峰值线程 | **10** |
| 峰值进程 | **25** |
| 结束时 RSS | 17,480 KB（约 17.1 MiB） |
| 结束时线程 | 10 |
| 结束时进程 | 21 |

说明：

- CPU 超过 100% 是 daemon 多线程在多核上的累计占用，不是单核上限；不包含 20 个插件子进程的 CPU。
- **进程数 21** = daemon 1 + 20 个 shell 插件；峰值 25 是 MCP 阶段 shell agent 的 `sed` 等短命子进程。
- **线程数始终 10**：daemon 主线程、插件 reactor、管理 reactor、HTTP reactor、4 个 HTTP worker、管理客户端 reactor、temp cleanup 线程。
- 峰值 RSS 约 30.5 MiB，结束时回落到约 17.1 MiB；SSE/WS 的连接状态和缓冲是主要波动来源。
- VM 约 583 MiB 是虚拟地址空间，不等于驻留内存。

## 解读

- 3 分钟持续 1000 客户端线程、每轮 1000 条 SSE/WS 连接，daemon 线程数没有增长，仍是 10。
- HTTP、MCP、SSE、WS 全部零失败，说明 reactor 路径在持续负载下没有出现连接泄漏、线程泄漏或明显的生命周期错误。
- MCP 的累计耗时（100.07 s）明显高于其他面，主要受 4 个 HTTP worker 串行转发管理通道限制；这不是连接建立瓶颈。
- HTTP 与 MCP 使用持久连接后，3 分钟内没有出现临时端口耗尽。
- SSE/WS 每轮 1000 条连接、每插件 50 条，低于每插件 64 条订阅上限，所以全部成功。

## 注意

- 这是 20 个最简 shell 插件的合成基准，不是生产容量承诺。
- 客户端 1000 个 OS 线程本身也消耗测试机资源；本报告的资源数字是 **daemon**，不含客户端。
- Windows 仍走 thread fallback，本结果只代表 Unix/WSL。
- 未覆盖插件内部模型/视频生成耗时、blob 大对象传输和 HTTP MCP progress 流。

## 复现

```bash
cargo build --release
python3 tools/perf/reactor_scale_soak.py \
  --binary target/release/svcd \
  --work /tmp/svcd-reactor-soak \
  --plugins 20 --threads 1000 --duration 180
```

输出 JSON，并写入 `<work>/results.json`。
