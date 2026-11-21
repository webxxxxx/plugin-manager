# tools/perf

Compatibility and performance harness for svcd's HTTP, MCP, WebSocket and SSE
surfaces. This is intentionally **not** part of `cargo test`: it starts a
release daemon, a Python echo plugin and real network clients. Performance runs
in `.github/workflows/perf.yml` (weekly/manual); the standards-based
compatibility suite is tracked in #106. See [docs/testing.md](../../docs/testing.md).

## Requirements

- Node.js 18+ and `npm`
- Python 3 (the fixture plugin uses a Python NDJSON agent)
- A release binary: `cargo build --release`

## Run

```sh
cd tools/perf
npm install
cd ../..
node tools/perf/run.mjs --compat   # protocol/client compatibility only
node tools/perf/run.mjs --perf     # HTTP/MCP/WS/SSE + mixed workload
node tools/perf/run.mjs --all      # both
node tools/perf/run.mjs --scale    # 0/5/10/30 plugins, process-tree RSS/CPU
python3 tools/perf/budget_scenario.py \
  --budget 50mb --plugins 10 --duration 60 \
  --per-plugin-concurrency 100               # 10 plugins x 100 workers, all five faces
```

Outputs:

- `target/perf-results.json` — raw compatibility checks and latency samples
- `target/perf-report.md` — tables for the current run
- `target/scale-results.json` / `target/scale-report.md` — 0/5/10/30 plugin scale run
- `target/budget-results.json` / `target/budget-report.md` — fixed-budget run:
  requested vs effective budget, derived limits, per-plugin per-surface traffic,
  RSS/CPU samples and the oversubscription probe

The harness creates `target/perf-run-<timestamp>/` with config, data, logs and
the bundled `perf.spk`. After start it sets `auth_passwd` through
`svc lan passwd`, so `/mcp` accepts `Bearer <password>`; it then mints a
`/plugin-ui/perf/{token}/` base through `POST /api/v1/plugins/perf/ui-token`
for the WS, SSE and `/rpc` checks. LAN stays disabled; the HTTP API remains
loopback-only and unauthenticated.

## Coverage

- HTTP: JSON/headers/length, keep-alive, 405 Allow, malformed request, chunked
  request rejection
- MCP: modern `server/discover` / `tools/list` / `tools/call`, Bearer 401/403,
  missing `_meta`, and the official SDK over the stdio bridge
- WebSocket: tools/call + notification, ping/pong/close, fragmentation,
  cross-origin 403, oversized close 1009
- SSE: EventSource delivery and `Last-Event-ID` replay
- Performance: HTTP/MCP list/call at concurrency 1/8/32 with a keep-alive agent,
  WS at 1/8/32 connections, SSE event throughput, 10s mixed workload, CPU/RSS
  sampling
- Scale: fresh daemon per 0/5/10/30 plugin count with a minimal cmd/sh script
  plugin (no Python); daemon and process-tree WorkingSet/CPU over a 5s idle
  window; start-all timing

## Windows-only manual blob/file benchmarks

`tools/perf/blob-bench/` holds the manual Windows PowerShell benchmarks for
blob and logical-file transfers: an ISO upload/download loop and a multi-part
file upload/download with the `filebench` plugin. They need a 1.59 GiB ISO,
PowerShell 7 and a release binary; they are **not** part of `run.mjs` or
`perf.yml`. See [blob-bench/README.md](blob-bench/README.md).

## Notes

- On Windows, CPU/RSS sampling uses PowerShell `Get-Process`; on Linux it reads
  `/proc` (including the plugin process tree).
- The Python fixture adds interpreter and JSON overhead to MCP/WS/SSE calls.
- `--scale` uses a minimal cmd/sh script plugin so daemon overhead is separated
  from interpreter overhead.
- `scale_cli.py` measures the minimal (`--no-default-features`) daemon through
  the CLI channel, where no HTTP API exists:

  ```sh
  python3 tools/perf/scale_cli.py --binary target/release/svcd \
    --work /tmp/min-scale --plugins 0,5,10,30 \
    --key samples/keys/sample-2026.key --pub samples/keys/sample-2026.pub
  ```
- `reactor_scale.py` is the Unix reactor regression: 20 shell `.spk` plugins,
  100 concurrent operations per plugin across HTTP/SSE/WS/MCP, with daemon
  thread sampling:

  ```sh
  python3 tools/perf/reactor_scale.py \
    --binary target/release/svcd --work /tmp/svcd-reactor-20 \
    --plugins 20 --concurrency 100
  ```

- `reactor_scale_threads.py` is the same workload with 1000 real client
  threads and daemon CPU/RSS/VM/thread/process sampling:

  ```sh
  python3 tools/perf/reactor_scale_threads.py \
    --binary target/release/svcd --work /tmp/svcd-reactor-1000 \
    --plugins 20 --threads 1000
  ```

- `reactor_scale_soak.py` cycles HTTP/MCP/SSE/WS for a sustained duration
  (default 180 s) and samples CPU/RSS/VM/threads/processes throughout:

  ```sh
  python3 tools/perf/reactor_scale_soak.py \
    --binary target/release/svcd --work /tmp/svcd-reactor-soak \
    --plugins 20 --threads 1000 --duration 180
  ```

- `memory_by_plugins.py` reports idle daemon RSS/VM/threads/processes for
  several plugin counts:

  ```sh
  python3 tools/perf/memory_by_plugins.py \
    --binary target/release/svcd --counts 0,3,10
  ```
- Minimal build: `cargo build --release --no-default-features`; see
  `scale_cli.py` for its resource test.
- In WSL, put the runtime work root on the Linux filesystem, not `/mnt/*`:
  `PERF_WORK_ROOT=/tmp/svcd-perf node tools/perf/run.mjs --scale`.
  Unix control sockets are not supported on the Windows filesystem mount.
- Compatibility HTTP checks use one-shot connections; performance workers use
  HTTP/1.1 keep-alive, so their numbers include connection reuse.
- Numbers are machine-specific and intended for relative comparison and
  bottleneck discovery, not as production capacity claims.
- `budget_scenario.py` requests an explicit `mem_budget_bytes` and drives every
  plugin through invoke (MCP `tools/call`), SSE, WebSocket, blob upload and blob
  download, then opens more streams than the budget admits to check that the
  refusals are 429/503 rather than a crash. `--per-plugin-concurrency N` pins N
  workers to each plugin (10 x 100 = 1000); `--traffic idle` only boots the
  plugins and samples RSS/CPU. `--plugin-runtime shell` uses a bash NDJSON agent
  on Linux (the default there; Python stays the Windows fixture).
  `--bootstrap direct` writes the registry layout before boot for hosts whose
  control channel is unavailable; it skips signature verification and staging,
  so leave it off on a normal host.
