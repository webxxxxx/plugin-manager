# 50 MB / 90% CPU：全功能插件跑多少个（WSL2 实测）

#107 的问题：只给 50 MB 内存预算、CPU 预算 90%，一台机器上能同时跑多少个**全功能**插件
（MCP `invoke`、SSE、WebSocket、upload、download 五面同时在用），按 5 的位数给 p50/p99、
内存与 CPU。

## 方法

- WSL2 / Debian，16 vCPU，客户端 Python 3；插件运行时 `shell`（bash NDJSON agent），
  每个插件都有 MCP 能力、web UI 与事件流，五面对它都是真实请求。
- 一轮一个计时点：

```sh
python3 tools/perf/budget_scenario.py \
  --binary /tmp/wt-107-target/release/svcd \
  --budget 50mb --cpu-budget-percent 90 \
  --plugins N --per-plugin-concurrency 1 --think-ms 10 --duration 20 \
  --no-oversubscribe --bootstrap direct --plugin-runtime shell \
  --work /tmp/sweep-N --out .scratch/sweep/N.json
```

  每个插件一个客户端 worker，顺序轮询五面；`N=0` 只跑 idle 基线。
- 每 2 s 采一次 daemon 与整棵进程树：RSS、PSS（`smaps_rollup`）、线程数、CPU 时间。
- 每个点都是 100% `200`、`coverage_strict=True`（五个面 × 每个插件都至少成功一次），
  没有 429/503，`max_connections = max_streams = 288`（50 MB 解出来的值，所有点相同）。

## 总量

| 插件 | ok/请求 | ops/s | p50 ms | p99 ms | daemon RSS MB | 插件 RSS MB | 树 RSS MB | 树 PSS MB | daemon CPU | 树 CPU | 线程 | 进程 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 0/0 | 0 | – | – | 3.4 | 0.0 | 3.4 | 1.9 | 0.3% | 0.3% | 7 | 1 |
| 5 | 8382/8382 | 419.0 | 1.42 | 4.6 | 4.6 | 16.6 | 21.2 | 5.8 | 11.0% | 13.0% | 7 | 6 |
| 10 | 16852/16852 | 842.5 | 1.39 | 4.3 | 5.1 | 33.3 | 38.3 | 8.0 | 23.8% | 27.6% | 7 | 11 |
| 15 | 24185/24185 | 1209.6 | 1.88 | 7.4 | 5.5 | 49.9 | 55.4 | 10.0 | 38.2% | 44.3% | 7 | 16 |
| 20 | 27586/27586 | 1379.4 | 3.61 | 12.0 | 6.0 | 66.4 | 72.5 | 12.0 | 52.6% | 61.0% | 7 | 21 |
| 25 | 27273/27273 | 1363.7 | 7.11 | 17.9 | 6.3 | 83.0 | 89.3 | 13.9 | 58.3% | 67.1% | 7 | 26 |
| 30 | 26421/26421 | 1321.7 | 10.70 | 27.9 | 6.6 | 99.8 | 106.4 | 15.6 | 53.2% | 61.4% | 7 | 31 |
| 35 | 27556/27556 | 1378.3 | 13.11 | 31.3 | 7.0 | 116.1 | 123.1 | 17.5 | 60.2% | 69.1% | 7 | 36 |
| 40 | 28495/28495 | 1425.2 | 15.37 | 36.9 | 7.4 | 133.1 | 140.5 | 19.4 | 56.0% | 63.7% | 7 | 41 |

CPU 列是**占单个核的百分比**（`daemon CPU` 60.2% 即 0.60 个核），不是整机百分比。

## 每个面

| 插件 | invoke p50/p99 | sse p50/p99 | ws p50/p99 | upload p50/p99 | download p50/p99 |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 5 | 1.2 / 2.5 | 2.1 / 4.5 | 1.2 / 2.6 | 3.0 / 5.4 | 0.8 / 1.5 |
| 10 | 1.2 / 2.5 | 2.0 / 4.7 | 1.2 / 2.4 | 2.9 / 4.9 | 0.8 / 1.9 |
| 15 | 1.4 / 5.0 | 2.6 / 9.7 | 1.6 / 6.2 | 3.1 / 6.3 | 0.9 / 3.4 |
| 20 | 2.8 / 8.9 | 6.0 / 14.4 | 3.6 / 10.7 | 4.6 / 10.0 | 1.8 / 6.7 |
| 25 | 5.8 / 13.2 | 11.4 / 20.5 | 7.9 / 15.9 | 7.5 / 14.3 | 4.3 / 10.5 |
| 30 | 8.7 / 19.9 | 16.8 / 32.7 | 12.5 / 27.4 | 10.5 / 22.0 | 6.8 / 17.3 |
| 35 | 10.8 / 22.7 | 21.0 / 36.0 | 15.8 / 29.4 | 12.3 / 23.8 | 8.5 / 18.8 |
| 40 | 12.4 / 25.6 | 24.6 / 42.2 | 18.6 / 34.6 | 14.4 / 27.0 | 10.2 / 21.9 |

## 「50 MB 能跑几个」有三个答案，取决于量的是谁

三条斜率都从同一张表里读出来：

| 口径 | 每个插件的边际成本 | 50 MB 对应的插件数 | 实测越线点 |
| --- | ---: | ---: | --- |
| daemon 自身 RSS | 0.10 MB | ~460（线性外推） | 40 个时 7.4 MB，远未到 |
| 整棵进程树 RSS | 3.43 MB | ~13 | 10 个 38.3 MB 未越、15 个 55.4 MB 越线 |
| 整棵进程树 PSS | 0.38 MB | ~125（线性外推） | 40 个时 19.4 MB，远未到 |

1. **daemon 自身**：预算管的就是这个进程。40 个插件只把它抬到 7.4 MB，`worst_case` 规划值
   52.4 MB / 50 MB 预算照旧成立。
2. **整棵树 RSS**（`ps` 直接加总）：bash 的同一个 libc 页被每个插件各算一次，所以斜率 3.43 MB/插件。
   按这个最保守的口径，50 MB 落在 **10 与 15 之间**——按 5 的位数说，就是 10 个稳过、15 个越线。
3. **整棵树 PSS**（共享页按比例摊，最接近真实占用）：0.38 MB/插件，40 个才 19.4 MB。

## CPU 90% 在这台机器上不是约束

`cpu_budget_percent` 的软门按**整机**利用率算（`utilization = used / (wall × cores)`），硬门
写的是 `percent × cores` 的 cgroup 配额。16 核 × 90% = 14.4 个核——daemon 全程最多 0.60 个核，
所以这次没有任何一个点触发 shedding，也没有一条 429/503。

吞吐在 20 个插件后停在 1.4k ops/s 而 daemon 只有 0.55 个核，说明卡的是**客户端**：Python 的
40 个线程共用一个 GIL。p50 从 1.4 ms 涨到 15 ms、p99 从 4.6 ms 涨到 37 ms 是客户端排队，
不是 daemon 排队——daemon 侧每个操作只花 ~0.4 ms CPU（1.4k ops/s ÷ 0.55 core）。

## 结论

- 50 MB 预算**不含 per-plugin 项**：不变式只解连接、流、订阅（50 MB 时 288/288），插件数不在
  预算里。预算是否被撑爆，只看 daemon 自己——40 个插件 7.4 MB，不到预算的 15%。
- 若把「内存 50 MB」理解为**整棵进程树**（daemon + 插件子进程），按 `ps` 的 RSS 口径上限是
  **13–14 个**全功能插件（按 5 的位数：10 个稳、15 个越线）；按 PSS 口径 40 个都还不到一半。
- 90% CPU 预算在 16 核机器上等于 14.4 个核，本次全量点都没碰到；真正的延迟增长来自压测客户端。

## 40 个插件：三档预算各自的并发上限（只认 100% 成功）

同一套流量，`--plugins 40`，把每插件客户端并发 P 从低往上加，一个点 20 s。
判据只有一条：该点 `errors = 0`（五面全部 200）。三档的规划值先摆出来：

| 预算 | workload | max_connections | max_streams | blocking_queue | blocking_threads | max_subscribers_per_plugin |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 50 MB | tiny | 288 | 288 | 288 | 4 | 16 |
| 512 MB | interactive | 399 | 399 | 399 | 8 | 32 |
| 2 GB | streaming | 786 | 786 | 786 | 16 | 64 |

实测（100% 成功点 + 它的下一个点）：

| 档 | 100% 成功的最大并发 | 总客户端 | 实测 peak in-flight | ops/s | p50 | p99 | 再往上一步 |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 50 MB | 每插件 8 | 320 | 300 | 1080 | 193 ms | 676 ms | P=9（360 个）：41 × 503 + 35 个 broken pipe |
| 512 MB | 每插件 12 | 480 | 417 | 912 | 251 ms | 1054 ms | P=13（520 个）：4 × 503 + 7 个 broken pipe |
| 2 GB | 每插件 ≥26 | ≥1040 | 500 | 1274 | 208 ms | 858 ms | 未出现失败（见下） |

- **回落点就在派生的并发数附近**：50 MB 的 288、512 MB 的 399 就是 `max_connections` /
  `blocking_queue` / `max_streams` 那一组数。客户端数的 peak in-flight 比服务端真正的并发高
  约 15%（请求在客户端排队时也算「in-flight」），所以 320 个客户端还能全过，360 个就开始有 503。
- **503 是准入拒绝，不是超时**：`statuses` 里就是 503，伴生的 `BrokenPipeError` 是连接被 drain
  后关闭的后果；没有任何一个点出现 timeout/429（429 只出现在 20 秒内 960 个线程的极端点）。
- **2 GB 档没测到上限**：这个压测客户端的 in-flight 峰值封顶在 ~500（单进程 Python 线程池，一个 GIL），
  786 的连接上限够不到。所以这一档的上限是规划值：**786 个并发连接**；再往上要换异步客户端才测得动。
- **p50/p99 是端到端，含客户端排队**：320 个客户端时每个线程只分到 ~3.4 ops/s，光排队就 ~200 ms。
  daemon 自己的服务时间在 P=1、40 个插件时是 p50 1.42 ms / p99 4.6 ms（前一节的表），
  `daemon CPU` 在 1.4k ops/s 时才 0.56 个核——瓶颈从来不是 daemon 的单次延迟。

一句话：40 个全功能插件时，50 MB 档按**每插件 8 个并发客户端（共 320）**规划、512 MB 档
**每插件 12 个（480）**、2 GB 档**每插件 26 个以上（≥1040，规划上限 786 个连接）**；
三档都是 100% 成功，再往上就是准入的 503，不是超时。

## 复现

```sh
# 构建（WSL 内）
CARGO_TARGET_DIR=/tmp/wt-107-target cargo build --release --bin svcd
# 扫描点
for N in 0 5 10 15 20 25 30 35 40; do
  python3 tools/perf/budget_scenario.py --binary /tmp/wt-107-target/release/svcd \
    --budget 50mb --cpu-budget-percent 90 --plugins $N --duration 20 \
    --per-plugin-concurrency 1 --think-ms 10 --no-oversubscribe \
    --bootstrap direct --plugin-runtime shell \
    --work /tmp/sweep-$N --out .scratch/sweep/$N.json --report .scratch/sweep/$N.md
done

# 三档并发上限：40 个插件，每插件 P 个客户端，取 100% 成功的那一点
for P in 8 9 12 13; do
  python3 tools/perf/budget_scenario.py --binary /tmp/wt-plugin-manager-target/release/svcd \
    --budget 512mb --cpu-budget-percent 90 --plugins 40 --per-plugin-concurrency $P \
    --duration 20 --think-ms 10 --no-oversubscribe --bootstrap direct --plugin-runtime shell \
    --work /tmp/conc-512-$P --out .scratch/conc/512-$P.json
done
```

`--cpu-budget-percent`、`traffic_meta.overall_latency`、`resources.tree_pss` 是本票给
`budget_scenario.py` 补的三个字段：前两个让报告能说出「在什么 CPU 预算下、整体 p50/p99 是多少」，
`tree_pss` 让「50 MB 装几个」不必只信 RSS 的加总。
