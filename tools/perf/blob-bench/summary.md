# Blob 5 并发上传/下载基准

- 日期：2026-11-15 20:46–20:48（Asia/Shanghai）
- 文件：`D:\learn\pve\i3-n305\proxmox-ve_9.2-1.iso`
- 大小：1,706,178,560 B = 1,627.1 MiB = 1.59 GiB
- 内容 SHA-256：`4e88fe416df9b527624a175f24c9aa07c714d3332afb1ee3dbf3879573ef2c6c`
- blob id：`sha256:4e88fe416df9b527624a175f24c9aa07c714d3332afb1ee3dbf3879573ef2c6c`
- 机器：16 logical CPU / 15.7 GB RAM，Windows，客户端与服务端同机（loopback）
- 并发：每轮 5 个 curl 进程同时发起；上传 = `POST /api/v1/blobs`，下载 = `GET /api/v1/blobs/{id}`
- 预热：测试前用 `Get-FileHash` 完整读一遍 ISO，因此上传阶段主要受 CPU/内存拷贝/loopback 限制，不是冷磁盘读
- 构建说明：ISO 超过生产上限 512 MiB。为跑完整的上传+下载，使用了一份临时源码副本，仅把 `MAX_BLOB_BYTES` 改为 4 GiB 后构建 release；生产源码未改。当前生产二进制对 1.59 GiB 的 `Content-Length` 直接返回 413。

## 结果（两轮）

| 指标 | 上传 5 并发 | 下载 5 并发 |
|---|---:|---:|
| 总传输量 | 5 × 1,627.1 MiB = 7,943 MiB | 5 × 1,627.1 MiB = 7,943 MiB |
| 墙钟耗时 | 9.221 s / 9.209 s | 6.200 s / 6.868 s |
| 聚合吞吐 | 882.3 / 883.4 MiB/s | 1,312.3 / 1,184.6 MiB/s |
| 单请求耗时 min/avg/max | 8.993–9.180 s（两轮） | 6.102–6.186 s / 6.717–6.846 s |
| daemon CPU 时间 | 29.094 s / 31.375 s | 29.453 s / 32.891 s |
| daemon 平均 CPU | 315.5% / 340.7%（≈3.2–3.4 核） | 475.1% / 478.9%（≈4.8 核） |
| daemon 峰值 CPU | 377.7% / 392.4% | 507.1% / 498.9% |
| daemon 峰值 Working Set | 8.3 / 7.7 MiB | 8.3 / 7.7 MiB |
| daemon 峰值 Private Bytes | 1.9 / 1.8 MiB | 1.9 / 1.9 MiB |
| daemon 线程/句柄峰值 | 12 / 130–136 | 12 / 130–136 |
| 系统 CPU 平均/峰值 | 48.5–52.5% / 68–73% | 39.6–48.7% / 55–73% |
| 进程文件 I/O | 写 8,135.7 MiB，读 0 | 读 8,135.7 MiB，写 0 |
| 成功请求 | 5/5 | 5/5 |

CPU 百分比按 Windows 习惯：单核满载 = 100%，多核可以超过 100%。

## 观察

- 流式路径的内存是**常数级**：5 个 1.59 GiB 并发传输时 daemon RSS 峰值约 8 MiB，Private Bytes 约 2 MiB，没有按文件大小增长。
- 上传阶段的 CPU 主要花在 5 份相同的 SHA-256 和 5 份 temp 文件写入上；内容寻址去重发生在 `finish`，所以 5 个相同文件仍会各写一份 temp（文件 I/O 8,135.7 MiB）。
- 下载阶段 daemon CPU 较高（约 4.8 核），说明 64 KiB 读/写循环在 loopback 高吞吐下是 CPU 密集的；单轮聚合约 1.2–1.3 GiB/s。
- 因为测试前预热了 ISO，页面缓存让上传不体现冷磁盘读；若要测冷缓存，需要在测试前清理系统文件缓存或重启。
- 生产环境要接收这个 ISO，需要把 blob 上限从 512 MiB 提高（代码里的 `MAX_BLOB_BYTES`，或做成配置项）；当前分片 `complete` 也受同一总上限约束。
