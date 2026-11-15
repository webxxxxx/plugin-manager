# MEM_LEVEL 资源占用：10 个 WebUI + WebSocket 插件

日期：2026-11-16  
平台：Windows x86_64-pc-windows-msvc + WSL2 Debian Linux x86_64  
构建：`cargo build --release`，`MEM_LEVEL=low|middle|high`  
时长：每档 180 秒，采样间隔 5 秒

## 方法

- 每档启动一个全新的 `svcd` daemon。
- 安装并启动 10 个插件（`p01`–`p10`）。每个插件：
  - 有 `payload/web/index.html`；
  - `exec` 是一个常驻循环（Windows 为 `cmd.exe`，WSL 为 `sh+sleep`），不退出；
  - 不处理业务请求，只用于测量基线和 10 条 WebSocket 连接。
- 每个插件申请一个 UI token，并建立 1 条 WebSocket（共 10 条），全程保持打开。
- Windows 测试把 `max_subscriptions` / `max_subscribers_per_plugin` 设为 64；
  WSL 还额外把 `max_connections` / `max_connections_per_ip` 设为 64，
  否则 low 档默认的 4 个订阅 / 8 个单 IP 连接无法承载 10 条 WS；其余配置用默认值。
- Windows daemon 内存取 `WorkingSet64`，插件内存取 10 个插件主进程 `WorkingSet64` 之和；
  WSL 用 `/proc/<pid>/status` 的 `VmRSS`，daemon 线程取 `Threads`。
- CPU 用 CPU 时间差值 / 180 秒，表示单个逻辑核的百分比。
- 结束时确认 `wsOpen = 10`。

## 结果

### Windows（10 个 `cmd.exe` 常驻插件）

| 档位 | daemon RSS 平均 | daemon RSS 峰值 | 10 插件 RSS 平均 | 10 插件 RSS 峰值 | 总 RSS 平均 | daemon 线程峰值 | daemon CPU | 插件 CPU |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| low | 10.01 MB | 10.12 MB | 72.99 MB | 74.33 MB | 83.00 MB | 39 | 0.57% | 0.08% |
| middle | 10.60 MB | 10.70 MB | 74.36 MB | 74.70 MB | 84.96 MB | 39 | 0.45% | 0.13% |
| high | 10.60 MB | 10.71 MB | 73.02 MB | 74.48 MB | 83.63 MB | 39 | 0.31% | 0.01% |

### WSL2 / Debian（10 个 `sh + sleep` 常驻插件）

WSL 上同样跑 10 个 WebUI 插件和 10 条 WebSocket，每档 180 秒。
插件内存用 `/proc/<pid>/status` 的 `VmRSS`，daemon 线程取 `Threads`。

| 档位 | daemon RSS 平均 | daemon RSS 峰值 | 10 插件 RSS 平均 | 总 RSS 平均 | daemon 线程峰值 | daemon CPU | 插件 CPU | WS 打开数 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| low | 4.51 MB | 4.70 MB | 16.50 MB | 21.00 MB | 30 | 0.37% | 0.00% | 10 |
| middle | 4.66 MB | 4.67 MB | 16.61 MB | 21.27 MB | 30 | 0.48% | 0.00% | 10 |
| high | 4.51 MB | 4.52 MB | 16.14 MB | 20.65 MB | 30 | 0.63% | 0.00% | 10 |

WSL 的插件是 `sh` 常驻循环，每个进程约 1.6 MB；Windows 的 `cmd.exe`
每个约 7.3 MB，所以两边插件内存不能直接横向比较，只能分别看 daemon 自身
和同一平台内的三档差异。

WSL 测试配置额外把以下上限调到 64，否则 low 档默认的
`max_connections_per_ip = 8` / `max_subscriptions = 4` 无法承载 10 条连接：

- `max_connections`
- `max_connections_per_ip`
- `max_subscriptions`
- `max_subscribers_per_plugin`

## 解读

- 三档的 daemon RSS 都在 10–11 MB；差异主要来自每线程栈的**虚拟保留**和
  运行时缓存，而不在于 `MAX_BODY`/package 上限：这些上限是上限值，不会预分配。
- 本场景的插件 RSS 占绝对大头：每个 `cmd.exe` 约 7.3 MB，10 个约 73 MB。
  这属于 Windows 进程基线，不代表 svcd 自身开销。
- 10 条 idle WebSocket 只增加少量 daemon 堆和连接线程；三档线程峰值都为 39。
- 三档 CPU 都在 1% 以下，属于 idle + 每 30 秒 WS ping 的基线。
- low 档适合更小的并发；本测试把订阅上限临时提高到 64，才得到可比较的 10 条 WS。
  按 low 默认值，最多 4 条全局订阅。

## 原始记录

每档的 JSON 和 daemon 日志在：

- `target/bench/run-low/summary.json`
- `target/bench/run-middle/summary.json`
- `target/bench/run-high/summary.json`
- `target/bench/bench-all.log`

WSL 的原始记录在：

- `/tmp/svcd-bench/run-low/summary.json`
- `/tmp/svcd-bench/run-middle/summary.json`
- `/tmp/svcd-bench/run-high/summary.json`
- `/tmp/svcd-bench/run-*/daemon.err`

## 备注

- 插件是常驻空循环，不是真实业务负载；真实插件的内存/CPU 需按插件自身
  payload 另测。Windows 的 `cmd.exe` 与 WSL 的 `sh+sleep` 进程基线不同，
  两边插件 RSS 不要横向比较。
- 本次还修正了一个低内存配置问题：流式安装/导入/导出/UI 静态资源线程原先使用
  `io_stack()`（low = 64 KiB），在 Windows 上会栈溢出。现在这些处理使用
  `transfer_stack()`（至少 256 KiB）；Windows 上不再额外 spawn 处理线程，而是在
  连接线程（同样使用 `transfer_stack()`）里直接完成；Unix reactor 仍按需 spawn
  四个流式处理 worker。
