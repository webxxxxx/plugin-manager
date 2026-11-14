# daemon 空闲内存：0 / 3 / 10 插件

> 日期：2026-11-14
> 环境：WSL2 Debian，release 构建
> 脚本：`tools/perf/memory_by_plugins.py`
> 采样：启动后等待 3 秒，插件已启动但无请求

## 结果

| 插件数 | RSS | VM | 线程 | 进程 |
|---:|---:|---:|---:|---:|
| 0 | **3,176 KB（约 3.1 MiB）** | 595,660 KB（约 582 MiB） | 10 | 1 |
| 3 | **3,340 KB（约 3.3 MiB）** | 595,660 KB（约 582 MiB） | 10 | 4 |
| 10 | **3,492 KB（约 3.4 MiB）** | 595,764 KB（约 582 MiB） | 10 | 11 |

## 解读

- 空闲 RSS 几乎不随插件数增长：0/3/10 插件只差约 300 KB，主要是插件管理结构和少量状态。
- 10 个线程是固定 reactor/worker 线程，不是每插件线程；0 插件时也已经存在。
- VM 约 582 MiB 基本恒定，是虚拟地址空间（glibc arena/线程栈预留等），不是驻留内存；看 RSS 才有意义。
- 进程数 = daemon 1 + 插件进程数；插件是独立 shell 进程，不占 daemon RSS。

## 与 3 分钟 soak 的 17.1 MiB 对比

3 分钟 soak 结束时 daemon RSS 为 **17,480 KB（约 17.1 MiB）**，高于空闲基线约 3.4 MiB。原因不是 17 MiB 的“固定池”，而是：

1. 1000 条 SSE/WS 连接和 HTTP/MCP 连接缓冲把堆推到约 30.5 MiB 峰值；
2. 连接关闭后对象释放，但 glibc malloc 通常保留已分配 arena/页，不立即归还 OS；
3. daemon 仍保留 10 个线程栈、日志/通道缓冲和 allocator 高水位；
4. VM 不变不代表 RSS 没释放：RSS 已从峰值 30.5 MiB 降到 17.1 MiB。

判断泄漏要看持续负载后是否继续单调增长；当前是“峰值 30.5 → 结束 17.1”，属于高水位保留，不是线程/连接泄漏。进程退出后 OS 会回收全部 RSS。

## 复现

```bash
cargo build --release
python3 tools/perf/memory_by_plugins.py \
  --binary target/release/svcd --counts 0,3,10
```
