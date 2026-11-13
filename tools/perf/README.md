# tools/perf

Compatibility and performance harness for svcd's HTTP, MCP, WebSocket and SSE
surfaces. This is intentionally **not** part of `cargo test` or CI: it starts a
release daemon, a Python echo plugin and real network clients.

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
```

Outputs:

- `target/perf-results.json` — raw compatibility checks and latency samples
- `target/perf-report.md` — tables for the current run
- `target/scale-results.json` / `target/scale-report.md` — 0/5/10/30 plugin scale run

The harness creates `target/perf-run-<timestamp>/` with config, data, logs and
the bundled `perf.spk`. It writes a test token into `config/lan-token` before
start so `/mcp` works without root; LAN stays disabled.

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

## Notes

- On Windows, CPU/RSS sampling uses PowerShell `Get-Process`; on Linux it reads
  `/proc` (including the plugin process tree).
- The Python fixture adds interpreter and JSON overhead to MCP/WS/SSE calls.
- `--scale` uses a minimal cmd/sh script plugin so daemon overhead is separated
  from interpreter overhead.
- In WSL, put the runtime work root on the Linux filesystem, not `/mnt/*`:
  `PERF_WORK_ROOT=/tmp/svcd-perf node tools/perf/run.mjs --scale`.
  Unix control sockets are not supported on the Windows filesystem mount.
- Compatibility HTTP checks use one-shot connections; performance workers use
  HTTP/1.1 keep-alive, so their numbers include connection reuse.
- Numbers are machine-specific and intended for relative comparison and
  bottleneck discovery, not as production capacity claims.
