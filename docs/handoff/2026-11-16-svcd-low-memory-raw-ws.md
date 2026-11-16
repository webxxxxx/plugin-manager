# svcd 低内存 / 流式传输 / raw WebSocket 交接

日期：2026-11-16  
分支：`feat/mem-levels-slice1-5`  
基线：`1e2d90b`  
工作区：`D:\learn\harmess_workspace\plugin-manager`

## 1. 交接摘要

当前分支已经完成：

- `MEM_LEVEL=low|middle|high` 和三档资源默认值；
- 日志、线程栈、HTTP/WS/SSE 连接数、body/header/stream 等配置；
- `.spk` 流式安装、`/api/v1/import` 流式导入；
- `.spk` / `.spkb` / config 流式导出；
- 插件 UI 静态资源流式发送；
- exec-ready 插件 stdout/stderr 进入 reactor；
- raw binary WebSocket；
- protocol 3 raw 插件传输（Unix）；
- 相关配置注释、压测报告和 AGENTS.md 生产级规则。

**尚未完成生产验收：**

- 1000 条 raw WS 连续 512 KiB 压测仍有 **58 个错误 / 60 条连接关闭**；
- protocol 3 raw 仍是每插件 writer/reader 两条线程，Linux 观察总线程 **70**，不是目标 10；
- Windows 没有 `/raw` 路由；
- 全局 raw 背压和错误分类尚未完成。

## 2. 提交

```text
c54175d feat: 适配低内存设备的 MEM_LEVEL 与流式插件传输
abfef10 docs(perf): 记录 20 个普通 spk 插件的三档资源占用
639b53f perf: 让 exec-ready 插件日志进入 reactor，减少每插件线程
97137ee docs(perf): 记录 20 插件 x 50 WebSocket 的 1 分钟压测
1904e01 docs(perf): 记录 1000 WebSocket 连续数据传输压测
b891adc docs(perf): 记录 1000 WebSocket 512KiB JSON echo 压测
28a9dae docs(perf): 记录 1000 WebSocket 512KiB 连续 echo 压测
53554b8 feat(ws): 支持 raw binary 回显并增加 WS 缓冲上限配置
02790c3 docs(perf): 记录 raw binary WebSocket 512KiB 连续压测
4f76b65 docs(perf): 记录 protocol 3 raw 插件 1000 连接压测
1c37ca2 feat(raw): protocol 3 length-framed raw plugin transport
26839c9 fix(raw): 避免关闭插件时 inflight 锁重入
```

`26839c9` 之后只完成了交接文档，不再改业务代码。

## 3. 改动清单

### 3.1 低内存配置

- [build.rs](build.rs:20)：`MEM_LEVEL` → `cfg(mem_low/mem_middle/mem_high)`。
- [src/limits.rs](src/limits.rs:23)：三档默认值。
- [src/settings.rs](src/settings.rs:19)：新增配置解析。
- [src/threads.rs](src/threads.rs:10)：请求/IO/transfer 栈。
- [src/log.rs](src/log.rs:169)：日志目录、64 KiB 轮转、`log_to_file`、反向 tail。
- [svcd.conf.example](svcd.conf.example:54)：三档配置表。
- [src/install.rs](src/install.rs:251)：生成的默认配置注释。

已有配置：

```text
logs_dir
log_max_bytes = 65536
log_rotations
log_to_file
request_stack_bytes
io_stack_bytes
max_body_bytes
max_json_body_bytes
max_complete_body_bytes
max_header_bytes
max_streams
stream_buffer_bytes
max_connections
max_connections_per_ip
max_subscriptions
max_subscribers_per_plugin
http_idle_timeout_secs
max_package_bytes
transfer_timeout_secs
ws_max_message_bytes
ws_inflight_bytes
ws_inflight_requests
ws_buffer_shrink_bytes
ws_raw_enabled
ws_raw_max_bytes
ws_raw_queue
```

### 3.2 流式安装/导入/导出

- [src/package.rs](src/package.rs:388)：`StreamTar`、`EntryReader`、`install_stream`。
- [src/plugin.rs](src/plugin.rs:1500)：staged 安装、导出流式。
- [src/install_http.rs](src/install_http.rs:23)：`.spk` 上传、`/api/v1/import` 流式。
- [src/export_http.rs](src/export_http.rs:22)：`.spk` / `.spkb` / config 流式导出。
- [src/ui_http.rs](src/ui_http.rs:33)：插件 UI 静态资源流式发送。
- [src/daemon.rs](src/daemon.rs:780)：`plugin.install_staged` RPC。

### 3.3 raw WebSocket

- [src/api.rs](src/api.rs:245)：`RawWsState`。
- [src/api.rs](src/api.rs:1310)：`prepare_raw_ws`。
- [src/api.rs](src/api.rs:1384)：`start_raw_ws`。
- [src/api.rs](src/api.rs:1575)：`drive_raw_ws`。
- [src/api.rs](src/api.rs:1640)：`handle_ws_frame` 的 raw binary 分支。
- [src/uichannel.rs](src/uichannel.rs:484)：Windows 侧的 binary 回显。

### 3.4 protocol 3 raw 插件

- [src/rawhub.rs](src/rawhub.rs:136)：`RawHub`。
- [src/rawhub.rs](src/rawhub.rs:166)：注册、writer/reader 线程。
- [src/rawhub.rs](src/rawhub.rs:288)：订阅。
- [src/rawhub.rs](src/rawhub.rs:314)：发送、全局预算。
- [src/slot.rs](src/slot.rs:416)：protocol 3 使用 piped stdin。
- [src/slot.rs](src/slot.rs:552)：raw 注册分支。
- [src/slot.rs](src/slot.rs:1293)：只收集 stderr 的日志线程。
- [src/daemon.rs](src/daemon.rs:136)：创建 RawHub 并传给 API。
- [src/package.rs](src/package.rs:28)：`MAX_PROTOCOL=3`。
- [src/package.rs](src/package.rs:1826)：protocol 3 只允许 `exec.ready=exec`，禁止 JSON-RPC capabilities。

protocol 3 帧格式：

```text
u32 payload_len (BE)
u32 stream_id   (BE)
payload
```

## 4. 验证结果

### Windows

```text
cargo test --test http                       45 passed
cargo test --bin svcd -- --test-threads=1   288 passed
cargo test --test daemon                    47 passed
```

### WSL

```text
cargo test --test http                       45 passed
cargo test --bin svcd -- --test-threads=1   305 passed
cli 15 / install 20 / mcp 10 / plugin 22    passed
daemon                                     45 passed / 4 failed
```

daemon 的 4 个失败是 WSL 环境/flaky：

```text
a_definition_added_while_running_is_picked_up_without_reload
a_marker_change_marks_an_installed_plugin_failed
a_second_daemon_refuses_to_start
a_stopping_record_is_finished_with_the_stop_action
```

基线 `1e2d90b` 在 WSL 也复现其中 3 个；`a_second_daemon...` 重复跑会 flaky。不是本次功能引入。

## 5. 遗留问题

### P0：raw 1000 连接仍有错误

最近一次结果：

```text
requests_completed  21466
data_ok             21464
errors              58
ws_open             940
```

结论：

- 已完成的 21464 条数据正确；
- 58 个连接错误，60 条连接关闭；
- 原因未定位到单一分类；
- 需要增加错误分类计数后再修：
  - raw writer queue full
  - subscriber queue full
  - budget exceeded
  - plugin EOF
  - WS write failure

### P0：Linux raw 线程数仍是 70，不是 10

`rawhub.rs` 每插件起：

```text
svcd-<id>-raw-write
svcd-<id>-raw-read
```

20 插件 → 40 条 raw 线程。  
reactor 只接入了 exec-ready **日志**，没有接入 protocol 3 raw 的 stdin/stdout。

必须在 `reactor.rs` 中增加：

- raw stdout 帧读取；
- raw stdin 可写队列；
- subscriber 投递；
- in-flight budget 释放；
- raw 插件 unregister。

然后删除 `rawhub.rs` 的 writer/reader 线程。

### P1：Windows raw 不支持

- `prepare_raw_ws` / `RawWsState` / `drive_raw_ws` 都在 `#[cfg(unix)]`。
- Windows 上 `/raw` 会走到静态 404。
- `RawHub` 本身跨平台，但 Windows 没有 raw WS 路由和 reactor 接入。

### P1：backpressure 不完整

- `RawHub` writer queue 已可配置 `ws_raw_queue`；
- subscriber queue 固定 4；
- 全局 `ws_inflight_bytes` / `ws_inflight_requests` 已存在；
- 但错误路径仍可能直接关连接，未做到“排队/限速/有界丢弃 + 分类日志”。

### P2：性能报告中的平台差异

- Windows 20 普通插件使用 `cmd.exe + ping`；
- WSL 使用 `tail -f /dev/null`；
- 插件 RSS 不能跨平台直接比较。

## 6. 复现

### 6.1 单元/HTTP 测试

Windows：

```powershell
$t = "D:\learn\harmess_workspace\plugin-manager\target\test-tmp"
New-Item -ItemType Directory -Force $t | Out-Null
$env:TEMP = $t
$env:TMP = $t
$env:CARGO_INCREMENTAL = "0"
$env:CARGO_BUILD_JOBS = "1"
cargo test --test http
cargo test --bin svcd -- --test-threads=1
```

WSL：

```bash
cd /mnt/d/learn/harmess_workspace/plugin-manager
cargo test --test http
cargo test --bin svcd -- --test-threads=1
```

### 6.2 protocol 3 raw 压测

本地脚本（未提交，位于 `target/bench/`）：

- `run_ws_raw_continuous.py`
- `raw_agent.py`

WSL 一次性准备并运行：

```bash
cd /mnt/d/learn/harmess_workspace/plugin-manager
ulimit -n 8192
base=/tmp/svcd-raw
rm -rf "$base"
mkdir -p "$base/bin" "$base/src" "$base/spk"
cargo build --release
cp target/release/svcd "$base/bin/svcd"
chmod +x "$base/bin/svcd"
cp "$base/bin/svcd" "$base/plugin"
chmod +x "$base/plugin"

for i in $(seq -w 1 20); do
  id=p$i
  d="$base/src/$id"
  mkdir -p "$d/payload/bin" "$d/payload/web"
  cp target/bench/raw_agent.py "$d/payload/bin/raw_agent.py"
  chmod +x "$d/payload/bin/raw_agent.py"
  printf '<!doctype html><html><body>%s</body></html>\n' "$id" > "$d/payload/web/index.html"
  cat > "$d/manifest.json" <<EOF
{"schema":3,"name":"$id","version":"1.0.0","desc":"raw plugin","protocol":3,"exec":{"path":"payload/bin/raw_agent.py","ready":"exec"},"interpreter":{"name":"python3"},"ui":{"type":"web","entry":"index.html"}}
EOF
  "$base/plugin" bundle "$d" --unsigned --id "$id" --version 1.0.0 \
    --desc "raw plugin" --exec payload/bin/raw_agent.py \
    --interpreter python3 -o "$base/spk/$id.spk" >/dev/null
done

ulimit -n 8192
python3 target/bench/run_ws_raw_continuous.py
```

脚本内测试配置：

```text
max_connections=4096
max_connections_per_ip=4096
max_streams=4096
max_subscriptions=2048
max_subscribers_per_plugin=64
ws_raw_enabled=on
ws_raw_max_bytes=1048576
ws_inflight_bytes=2147483648
ws_inflight_requests=2048
```

`raw_agent.py` 核心：

```python
import struct, sys
source = sys.stdin.buffer
sink = sys.stdout.buffer
while True:
    h = source.read(8)
    if len(h) < 8:
        break
    length, stream = struct.unpack("!II", h)
    payload = source.read(length)
    if len(payload) < length:
        break
    sink.write(struct.pack("!II", length, stream))
    sink.write(payload)
    sink.flush()
```

### 6.3 JSON echo 压测

脚本：`target/bench/run_ws_echo_continuous.py`  
报告：[512 KiB 连续 echo](docs/perf/2026-11-16-1000-ws-512k-echo-continuous.md)

### 6.4 普通插件三档压测

脚本：`target/bench/run_plain.ps1` / `run_plain_wsl.py`  
报告：[20 个普通 spk](docs/perf/2026-11-16-20-plain-spk.md)

## 7. 建议的下一步顺序

1. 在 `reactor.rs` 中接入 protocol 3 raw stdin/stdout，删除 `rawhub.rs` writer/reader 线程，把线程数降到 10。
2. 加 raw 错误分类计数和日志。
3. 修到 `ws_open=1000`、`errors=0`。
4. 补 Windows raw WS 或明确 Windows 返回 501/unsupported。
5. 重跑 20×50×512 KiB×60 秒，并更新报告。
6. 再跑 Windows/WSL 全量测试。

## 8. 关键风险

- 当前 branch 的 protocol 3 raw 是 **Unix 功能验证版**，不能按生产交付；
- 58 个错误未定位，不能只靠调大队列掩盖；
- 线程数 70 与 Linux reactor 目标 10 不符；
- Windows raw 是缺失能力；
- 压测脚本和 raw agent 在 `target/bench/`，未提交，交接时需保留该目录或复制到 `tools/perf/`。
