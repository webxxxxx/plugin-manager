# `/api/v1/file` 5 并发大文件基准（daemon + plugin，含零拷贝对比）

- 日期：2026-11-15 21:21–22:08（Asia/Shanghai）
- 文件：`D:\learn\pve\i3-n305\proxmox-ve_9.2-1.iso`，1,706,178,560 B = 1,627.1 MiB
- 文件 id：`sha256:4e88fe416df9b527624a175f24c9aa07c714d3332afb1ee3dbf3879573ef2c6c`
- 机器：16 logical CPU / 15.7 GB RAM，Windows，客户端与服务端同机 loopback
- 上传：5 个并发文件会话，每个文件 4 个 part = 3 × 512 MiB + 91.1 MiB；共 20 个 part PUT，单 part 上限仍是 **512 MiB**
- 下载：5 个并发 `GET /api/v1/file/{id}`，客户端写 `NUL`
- 插件：`filebench` Python 插件，通过 `$file` 读 manifest/parts 并计算 SHA-256
- 预热：ISO 先做一次 SHA-256，页缓存已热；文件包刚写完，下载/插件读取主要来自页缓存
- 生产代码：`/api/v1/blobs` 仍是 512 MiB；`/api/v1/file` 的 `max_file_bytes` 为 16 GiB，`max_file_part_bytes` 为 512 MiB

## 结果

| 指标 | 上传 5 并发（两轮） | 下载 5 并发（两轮） | 插件消费（两轮） |
|---|---:|---:|---:|
| 墙钟耗时 | 14.039 / 18.829 s | 6.091 / 7.766 s | 1.653 / 1.633 s |
| 聚合吞吐 | 579.5 / 432.1 MiB/s | 1,335.7 / 1,047.6 MiB/s | — |
| 单请求耗时 min/avg/max | 13.20–13.56 / 17.89–18.35 s | 6.01–6.07 / 7.63–7.75 s | — |
| 成功请求 | 5/5 / 5/5 | 5/5 / 5/5 | sha 匹配 / sha 匹配 |
| daemon CPU 时间 | 40.453 / 49.328 s | 26.750 / 34.656 s | 0.0 / 0.0 s |
| daemon 平均 CPU | 288.2% / 262.0%（≈2.6–2.9 核） | 439.2% / 446.3%（≈4.4 核） | ≈0 |
| daemon 峰值 CPU | 446.1% / 491.9% | 512.8% / 499.9% | 0% |
| daemon 峰值 Working Set | 8.6 / 8.5 MiB | 8.6 / 8.5 MiB | 8.2 / 8.2 MiB |
| daemon 峰值 Private Bytes | 2.2 / 2.2 MiB | 2.2 / 2.2 MiB | 2.1 / 1.9 MiB |
| daemon 线程/句柄峰值 | 15 / 146；14 / 145 | 15 / 145；14 / 145 | 11 / 129；8 / 128 |
| 系统 CPU 平均/峰值 | 50.1% / 83%；46.3% / 92% | 38.5% / 62%；40.8% / 65% | 14.5% / 19%；12.3% / 24% |
| daemon 文件 I/O | 读 8,135.7 MiB / 写 8,135.7 MiB | 读 8,135.7 MiB / 写 0 | 读 0 |
| 插件 CPU 时间 | — | — | 1.578 / 1.609 s |
| 插件平均/峰值 CPU | — | — | 95.5% / 99.7%；98.6% / 99.3%（≈1 核） |
| 插件峰值 Working Set / Private | — | — | 13.3–14.2 MiB / 7.9–8.0 MiB |
| 插件文件 I/O | — | — | 读 1,627.1 MiB（两轮） |
| 插件校验 | — | — | bytes=1,706,178,560，sha256 匹配（两轮） |

CPU 按 Windows 习惯：单核满载 = 100%，多核可超过 100%。系统 CPU 包含 5 个 curl/worker 客户端和 OS；daemon 列是服务进程本身。

## 解读

- **两轮 wall 有 10–30% 波动**，CPU 时间也随系统频率/后台负载变化；但内存、插件 CPU、I/O 量级稳定。下文除特别说明外取第一轮（14.0 s / 6.1 s）为主，第二轮作为重复性验证。
- **daemon 内存仍然是常数级**：5 个 1.59 GiB 文件并发上传/下载时，RSS 峰值约 8.6 MiB，Private Bytes 约 2.2 MiB；文件包没有把整个文件读入内存，也没有在 `complete` 时复制出一份大文件。
- **上传多了一次整文件哈希**：part 上传本身写 8.1 GiB，`complete` 为了内容寻址再顺序读 8.1 GiB 计算整文件 SHA-256，所以 daemon 文件 I/O 是 8.1 GiB 读 + 8.1 GiB 写，墙钟 14.0 s（此前 5 个单 blob 上传约 9.2 s / 882 MiB/s）。这是文件包内容寻址的代价；如果后续更看重吞吐，可以考虑客户端提供整文件哈希、Merkle/part 根哈希，或可选的“跳过整文件哈希”模式。
- **5 个相同文件仍然各写一份 part 临时数据**，到 `complete` 才按整文件 SHA-256 去重；峰值临时磁盘约 5 × 1.59 GiB，最终只保留一份文件包。客户端可先 `HEAD /api/v1/file/{sha256}` 避免重复上传。
- **插件与 daemon 的资源边界很清楚**：插件消费阶段 daemon CPU ≈ 0，插件自身约 1 核、1.58 s、RSS 13.3 MiB；插件读了 1,627.1 MiB 并独立算出同样的 SHA-256。插件没有把文件复制进自己的内存。
- **下载和之前的 blob Range 路径接近**：5 并发 1,335.7 MiB/s，RSS 8.6 MiB；文件包按 part 顺序流式输出，Range 可跨 part。
- 所有数字是热页缓存下的 loopback 测试，不代表冷磁盘或真实网络。

## 与旧 `/api/v1/blobs` 基线对比

| 指标 | 旧 blob（5 并发，临时 4 GiB 构建） | 新 file（5 并发，生产 16 GiB 上限） |
|---|---:|---:|
| 上传墙钟 | 9.221 s | 14.039 s（另一轮 18.829 s） |
| 上传聚合吞吐 | 882.3 MiB/s | 579.5 MiB/s（另一轮 432.1） |
| 上传 daemon 平均 CPU | 315.5% | 288.2%（另一轮 262.0%） |
| 上传 daemon 峰值 RSS | 8.3 MiB | 8.6 MiB（另一轮 8.5） |
| 下载墙钟 | 6.200 s | 6.091 s（另一轮 7.766 s） |
| 下载聚合吞吐 | 1,312.3 MiB/s | 1,335.7 MiB/s（另一轮 1,047.6） |
| 下载 daemon 峰值 RSS | 8.3 MiB | 8.6 MiB（另一轮 8.5） |

新 file 路径的上传变慢主要来自「part 写入 + 整文件哈希」两次遍历，而不是内存/流式实现退化；下载两者相当。

## 零拷贝改造后（仅保留有收益的路径）

2026-11-15 22:22 的最终 release 构建（`run-file-nommap`）：

| 指标 | 上传 5 并发 | 下载 5 并发 | 插件消费 |
|---|---:|---:|---:|
| 墙钟耗时 | 22.508 s | 10.196 s | 1.557 s |
| 聚合吞吐 | 361.5 MiB/s | 797.9 MiB/s | — |
| daemon CPU 时间 | 48.078 s | 0.031 s | 0.016 s |
| daemon 平均 / 峰值 CPU | 213.6% / 449.6% | 0.3% / 2.9% | 1.0% / 2.9% |
| daemon 峰值 Working Set / Private | 8.6 / 2.2 MiB | 8.6 / 2.2 MiB | 8.2 / 2.0 MiB |
| daemon 文件 I/O | 读 8,135.7 / 写 8,135.7 MiB | 读 0 / 写 0 | 读 0 |
| 插件 CPU / RSS | — | — | 1.516 s（97.3%）/ 13.2 MiB |

- **上传恢复到基线资源形态**：RSS 8.6 MiB、读写各 8.1 GiB，CPU 48.1 s（在改造前 40.5–49.3 s 范围内）。墙钟 22.5 s 偏慢，但五个请求的 CPU 时间和 I/O 量级与基线一致，属于当前系统/磁盘波动；去掉了 `mmap` 哈希和映射上传后不再有额外的映射页 RSS。
- **下载仍保留 `TransmitFile`（Windows）/ `sendfile`（Linux）**：daemon CPU 从约 4.4 核降到约 0；代价是 Windows loopback 墙钟 10.2 s（798 MiB/s），比改造前 6.1–7.8 s 慢。这是用吞吐换 CPU，客户端/内核路径成为瓶颈。
- **已移除、没有配置开关**：映射上传（预分配 + 可写 `mmap` 窗口）和文件包/`import_file` 的 `mmap` 哈希。前者实测 21.5–24.7 s、CPU 66–69 s、RSS 48 MiB；后者把并发完成的 RSS 从 8.6 MiB 抬到 15.1 MiB。两者都没有保留开关，代码已删除。
- `$blobPath` 结果导入仍是原地哈希 + `rename`；chunked/multipart 解码仍直接消费切片。这两项不在本基准的上传/下载路径里，因此不改变上表数字。
- 大 Range（2 MiB 文件取 1 MiB–2 MiB）和跨 part Range 已在集成测试中覆盖 `sendfile` 偏移。

## 相关文件

- 基准结果：`target/blob-bench/run-file/result.json`（两轮基线）、`target/blob-bench/run-file-nommap/result.json`（最终无映射配置）
- 基准脚本：`target/blob-bench/run-file-bench.ps1`
- 上传 worker：`target/blob-bench/upload-file-worker.ps1`
- 插件源码：`target/blob-bench/plugin-src/`，包：`target/blob-bench/filebench.spk`
- 实现：`src/blob.rs`（文件包/哈希/清理/`$file`）、`src/blob_http.rs`（文件 API/跨 part Range）、`src/api.rs`（路由）
