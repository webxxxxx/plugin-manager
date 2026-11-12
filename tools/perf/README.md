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
```

Outputs:

- `target/perf-results.json` — raw compatibility checks and latency samples
- `target/perf-report.md` — tables for the current run

The harness creates `target/perf-run-<timestamp>/` with config, data, logs and
the bundled `perf.spk`. It writes a test token into `config/lan-token` before
start so `/mcp` works without root; LAN stays disabled.

## Coverage

- HTTP: JSON/headers/length, 405 Allow, malformed request, chunked request
  rejection, `Connection: close`
- MCP: modern `server/discover` / `tools/list` / `tools/call`, Bearer 401/403,
  missing `_meta`, and the official SDK over the stdio bridge
- WebSocket: tools/call + notification, ping/pong/close, fragmentation,
  cross-origin 403, oversized close 1009
- SSE: EventSource delivery and `Last-Event-ID` replay
- Performance: HTTP/MCP list/call at concurrency 1/8/32, WS at 1/8/32
  connections, SSE event throughput, 10s mixed workload, CPU/RSS sampling

## Notes

- On Windows, CPU/RSS sampling uses PowerShell `Get-Process`; on other platforms
  the resource block is reported as unavailable.
- The Python fixture adds interpreter and JSON overhead to MCP/WS/SSE calls.
- HTTP uses `Connection: close`; each HTTP/MCP measurement includes TCP setup.
- Numbers are machine-specific and intended for relative comparison and
  bottleneck discovery, not as production capacity claims.
