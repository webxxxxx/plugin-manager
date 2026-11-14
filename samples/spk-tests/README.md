# WSL sample `.spk` packages

Five small signed packages used to exercise the surfaces a WSL install depends
on. Sources live here; `plugin bundle` turns each directory into a signed
`.spk` whose `platform` is the target of the svcd binary that ran the bundle
(so a binary built inside WSL accepts the result).

| Directory | Package id | What it proves |
| --- | --- | --- |
| `dep-base/` | `dep-base` | plain notify-ready plugin with one capability |
| `dep-app/` | `dep-app` | `dependencies: ["dep-base"]`; start refuses until the base exists |
| `mcp-echo/` | `mcp-echo` | `echo` / `add` / `status` capabilities on `POST /mcp` and the stdio `mcp` bridge |
| `ws-panel/` | `ws-panel` | WebSocket downlink + `tools/call` uplink, with a live web UI |
| `sse-panel/` | `sse-panel` | SSE downlink + `POST /rpc` uplink, with `Last-Event-ID` replay in the UI |

All payloads are Python 3 scripts (`interpreter: python3 >= 3.8`), so WSL only
needs `python3`; there is no native build step per package.

## Build

Build svcd inside WSL first, then bundle the samples with that same binary. The
bundler rewrites `platform` to its own target triple, so the package is only
installable by a daemon built for the same target (WSL on x86_64 is
`x86_64-unknown-linux-gnu`, which is what the checked-in manifests declare).

```sh
cd /mnt/d/learn/harmess_workspace/plugin-manager
cargo build

# `plugin` is a multicall entry name; use the symlink created for the test.
ln -sf target/debug/svcd /tmp/plugin
cd samples/spk
/tmp/plugin bundle ../spk-tests/dep-base --key ../keys/sample-2026.key --key-id sample-2026
# ... repeat for dep-app, mcp-echo, ws-panel, sse-panel
```

`tools/spk_smoke.py` does the bundling for you and writes the packages to
`samples/spk/`.

The signing key is generated once (and is git-ignored):

```powershell
node tools/gen_sample_key.mjs
```

## Test

```sh
python3 tools/spk_smoke.py --binary target/debug/svcd
```

The harness uses the documented password writer (`svc auth passwd`) after
starting the daemon, so it needs root or a debug build (where the test-only
`SVCD_TEST_UNPRIVILEGED=1` bypass is compiled in; the harness sets it). Use a
release binary only when running the smoke as root.

The harness creates a throwaway config/data/log tree, trusts
`samples/keys/sample-2026.pub`, installs the packages, and checks:

1. **Dependencies** — `dep-app` installs without `dep-base`, refuses to start
   (`missing dependencies: dep-base`), then starts `dep-base` first once it is
   installed. `svc status dep-app --json` records the edge; stopping `dep-app`
   does not stop `dep-base`.
2. **MCP** — modern `tools/list` / `tools/call` over `POST /mcp` with a Bearer
   password, 401/403 checks, plus the dual-era stdio bridge (modern `_meta` and
   a legacy `initialize` handshake).
3. **WebSocket** — `/plugin-ui/ws-panel/ws` handshake, one notification per
   second, `tools/call status`, `emit`, ping/pong and a close frame. A plain
   `GET /ws` without the Upgrade header is rejected with 426.
4. **SSE** — `/plugin-ui/sse-panel/events` events with ids, `POST /rpc` uplink,
   and a reconnect with `Last-Event-ID` replaying what was missed.

It exits non-zero if a check fails and writes `target/spk-smoke.md` plus
`target/spk-smoke.json`. Pass `--keep` to keep the generated config tree.

## Watch the web UIs

Keep the samples running and open the pages from the Windows browser:

```sh
python3 tools/spk_smoke.py --binary target/debug/svcd --serve --port 18787
```

- `http://127.0.0.1:18787/plugin-ui/ws-panel/` — one notification per second on
  the socket, an emit round-trip, disconnect/reconnect.
- `http://127.0.0.1:18787/plugin-ui/sse-panel/` — live `EventSource` stream;
  press **Disconnect**, **Burst 5**, then **Replay missed (Last-Event-ID)** to
  watch the daemon replay the gap.

If Windows cannot reach the WSL loopback address, use `--serve --lan`; the UI
then asks for the smoke password (`spk-smoke`) and the printed URL uses the WSL
address.

## Add it to DSH as an MCP connection

The daemon's `/mcp` endpoint is modern Streamable HTTP and needs a Bearer
password. The harness reports the live port and password; a `dsh-mcp-link`
entry looks like:

```yaml
- insert:
    - id: mcp-link-svcd-spk
      name: "@deepseek-ai/dsh-mcp-client"
      config:
        serverName: svcd_spk
        transport: streamable-http
        url: http://127.0.0.1:18787/mcp
        headers:
          Authorization: Bearer spk-smoke
```

Start the daemon with `--serve` before the client connects; `tools/list` then
contains `mcp-echo.echo`, `mcp-echo.add`, `mcp-echo.status` and the `<id>.start`
entry for every installed service. This endpoint was exercised with the exact
client library DSH ships (`@modelcontextprotocol/client` 2.2.0 with
`versionNegotiation: { mode: "auto" }`): it negotiated `2026-07-28`, listed all
14 tools and returned `mcp-echo: hello-from-dsh-mcp-client` from `tools/call`.

## Notes

- Dependency resolution happens at **start**, not at install: installing
  `dep-app` alone succeeds, but starting it reports the missing base.
- The daemon starts dependencies in earlier layers and does not stop them when
  the dependent stops.
- These packages use loopback authentication as configured by the harness; in
  `--serve` mode LAN auth is off unless `--lan` is passed.
- The UIs are served by svcd itself; the plugin never opens its own HTTP
  listener. The SSE/WebSocket pair here is the **plugin UI channel**; the MCP
  `/mcp` transport is JSON-only and unrelated to it.
- Each package is deliberately self-contained: a `.spk` can only ship files
  inside its own directory, so the small JSON-RPC boilerplate is repeated in
  each agent rather than shared from outside the bundle.
