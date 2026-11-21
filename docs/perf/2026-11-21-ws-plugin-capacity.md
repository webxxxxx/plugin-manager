# 只开 WebSocket 的插件：三档预算各能同时跑多少个（WSL2 实测）

前一张表压的是「五面全开」的插件（invoke / SSE / WebSocket / upload / download）。
这次只留一面：**每个插件一条常驻 WebSocket 连接、同一时刻一个在飞请求（单并发）**，
问的是同一个问题——50 MB、512 MB、2 GB 三档预算，各自能装多少个这样的插件、同时跑起来。

## 方法

- WSL2 / Debian，8 vCPU，5.8 GiB 内存，Python 3.11；daemon 是 `v0.1.2` release 构建，
  `cpu_budget_percent = 90`。
- 夹具是 [budget_scenario.py](../../tools/perf/budget_scenario.py) 里那个 bash NDJSON agent，
  每个插件实例一个进程；另跑一组真包 `samples/spk-tests/ws-panel`（Python，自带每秒 tick）做对照。
- 每个插件的连接就是它自己的 web UI 会开的那条：
  `POST /api/v1/plugins/<id>/ui-token` 铸 token，`GET /plugin-ui/<id>/<token>/ws` 拿 101，
  之后每 1 s 在这条 socket 上发一个 `tools/call`，收到结果才发下一个——**任何时刻每个插件只有 1 个在飞请求**。
- 判据只有一条：N 条连接全部 101、所有调用都成功、没有 timeout / 断链。差一条就算这一档装不下。
- 每 2 s 采一次 daemon 自身（RSS / 线程）与整棵进程树（PSS / RSS / 进程数）。

```sh
python3 tools/perf/ws_plugin_capacity.py \
  --binary /tmp/wt-1012-target/release/svcd \
  --budget 50mb --plugins 288 --duration 20 --think-ms 1000 \
  --work /tmp/ws-cap-50mb-288 \
  --out .scratch/ws/50mb-288.json --report .scratch/ws/50mb-288.md
```

## 三档结果

| 预算 | workload | max_connections | **100% 成功的插件数** | 再上一个点 | daemon RSS 峰值 | daemon 线程 | 进程数 | 整树 PSS | 整树 RSS | p50 / p99 |
| ---: | --- | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 50 MB | tiny | 288 | **288** | 289 个：1 条 503 | 25.8 MB | 7 | 289 | 105.1 MB | 981.5 MB | 12.9 / 86.2 ms |
| 512 MB | interactive | 399 | **399** | 400 个：1 条 503 | 34.2 MB | 11 | 400 | 143.4 MB | 1356.8 MB | 11.8 / 95.2 ms |
| 2 GB | streaming | 786 | **786** | 787 个：1 条 503 | 63.3 MB | 19 | 787 | 277.0 MB | 2666.5 MB | 16.9 / 258.3 ms |

三档的「100% 成功的插件数」都**正好等于派生的 `max_connections`**：一个插件一条常驻 WS，
一条 WS 就是一个连接名额，288 / 399 / 786 是什么数，插件就是多少。

再往上的那一个点，拒绝形态固定：第 N+1 个插件挂在 `POST /api/v1/plugins/<id>/ui-token` 或 WS 握手上，
返回 **503（准入拒绝）**，不是超时、不是 429；已经建立的 N 条连接照旧 100% 成功。

## 内存不是约束，连接数才是

| 档 | daemon RSS | 占预算 | 每插件 daemon 成本（扣 3.4 MB 基线） | daemon CPU 峰值 |
| ---: | ---: | ---: | ---: | ---: |
| 50 MB | 25.8 MB | 52% | 78 KB | 4.4% 单核 |
| 512 MB | 34.2 MB | 6.7% | 79 KB | 6.2% 单核 |
| 2 GB | 63.3 MB | 3.1% | 76 KB | 14.6% 单核 |

每个插件（注册表项 + 一条常驻连接 + 它的订阅）在 daemon 里约 **77 KB**，三档一条直线；
正是这条斜率让人可以按连接数而不是按内存估算。50 MB 档跑满 288 个也只用了预算的一半，
`worst_case` 规划值 52.4 MB 仍然成立。

整棵树是另一回事：bash 夹具每个实例约 3.4 MB RSS（且共享页各算一次），288 个 981 MB、
786 个 2.67 GB。预算不管插件进程，这一列只是提醒「50 MB 的 daemon 可以带起 1 GB 的插件树」。

## 边界与两个容易误判的点

- **512 MB 档第一次跑 399 只开了 398**：第 399 个插件的 WS 握手拿到一条 503。原因是
  `/ui-token` 那次 POST 自己也要占一个连接名额，收尾与握手在同一个槽位上撞了一次；重跑一次就是
  399/399 全过（上表用的是重跑数据），再往上到 400 个才稳定出现 1 条拒绝。这是边界抖动，
  不是 398 的上限。
- **256 个页面 token 的上限不挡这个场景**：token 只在握手时校验
  （`src/face/plugin_streams/mod.rs` 的 `admit` → `src/face/plugin_ui.rs` 的 `valid_token`），
  `plugin_tokens` 满了会驱逐最早的一条（`src/server/auth/lan.rs`），但已建立的连接不受影响，
  所以 786 个插件可以各自握手成功。真实后果在另一处：**超过 256 个插件 UI 同时重连时，
  先重连的那批会拿到 401**——这是页面 token 的容量，不是连接的容量。
- **线程数与 CPU 都不按连接涨**：288 条连接 7 线程、399 条 11 线程、786 条 19 线程（Unix 是
  poll 循环 + 少量 blocking 线程，不是每连接一线程）；786 条连接每秒 786 次调用，daemon 峰值
  只占 14.6% 单核。三档都是准入先到顶，不是 CPU 或线程先到顶。

## 真包对照：ws-panel

`samples/spk-tests/ws-panel` 是仓库里打包好的 WebSocket 示例（Python，每秒主动推一条 tick，
额外提供 `status` 能力）。50 MB 档装 32 个、每个一条连接：

| 项 | 值 |
| --- | ---: |
| 连接 | 32/32，全部 101 |
| 调用 | 640/640 成功 |
| p50 / p99 | 5.1 / 11.6 ms |
| daemon RSS | 6.7 MB |
| 整树 PSS | 151.3 MB（每个插件约 4.6 MB，Python 比 bash 贵） |

它的协议路径与前面完全一样（`/plugin-ui/<id>/<token>/ws`），差别只在夹具语言与自带 tick。

## 结论

- 三档能同时跑的 WebSocket 插件数：**50 MB → 288、512 MB → 399、2 GB → 786**，全部 100% 成功；
  再上一个就是一条 503，不是超时。
- 换算关系简单：一条常驻 WS = 一个连接名额 = 约 77 KB daemon 内存；插件进程本身的 3.4 MB
  （bash）不算在预算里。
- 想在这一层再加容量，调的是预算（`mem_budget_bytes` / `mem_budget_percent`）与 workload 形状，
  不是插件数。

## 复现

```sh
# 构建（WSL 内，Linux 文件系统上）
CARGO_TARGET_DIR=/tmp/wt-1012-target cargo build --release --bin svcd

# 三档各自的满额点与越线点
for spec in "50mb 288" "50mb 289" "512mb 399" "512mb 400" "2gb 786" "2gb 787"; do
  set -- $spec
  python3 tools/perf/ws_plugin_capacity.py --binary /tmp/wt-1012-target/release/svcd \
    --budget $1 --plugins $2 --duration 20 --think-ms 1000 \
    --work /tmp/ws-cap-$1-$2 \
    --out .scratch/ws/$1-$2.json --report .scratch/ws/$1-$2.md
done

# 真包：ws-panel，32 个，50 MB
python3 tools/perf/ws_plugin_capacity.py --binary /tmp/wt-1012-target/release/svcd \
  --budget 50mb --plugins 32 --duration 20 --think-ms 1000 \
  --fixture samples/spk-tests/ws-panel --capability status \
  --work /tmp/ws-panel-32 --out .scratch/ws/50mb-32-panel.json
```

`--plugins` 是「装几个、开几条连接」，`--think-ms` 是同一条连接上两次调用的间隔（单并发就靠它：
上一个没回来不会发下一个）。退出码 0 表示这一档 N 个全过，2 表示有拒绝或错误——脚本可以直接当
容量门禁用。
