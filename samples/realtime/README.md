# Realtime UI sample

A runnable sample plugin that exercises both UI realtime transports from
the core spec section 3.3:

- SSE: GET /plugin-ui/realtime/events streams the agent's notifications.
  Uplink still uses POST /plugin-ui/realtime/rpc.
- WebSocket: GET /plugin-ui/realtime/ws carries tools/call requests and
  plugin notifications on one full-duplex connection.
- The Python agent emits a tick notification every two seconds and answers
  the echo capability.

## Build and run

    # The sample key is test-only and intentionally ignored by git.
    node tools/gen_sample_key.mjs

    # The multicall binary dispatches on argv[0], so the plugin entry is a copy/link.
    cp target/release/svcd target/release/plugin        # Windows: copy svcd.exe plugin.exe

    target/release/plugin bundle samples/realtime \
      --key samples/keys/sample-2026.key --key-id sample-2026 \
      -o target/realtime.spk

    # The installer trusts <config>/trust.d/<keyId>.pub. For a local test,
    # point the daemon and the CLI at the same override directories:
    export SVCD_CONFIG_DIR="$PWD/target/svcd-config"
    export SVCD_DATA_DIR="$PWD/target/svcd-data"
    mkdir -p "$SVCD_CONFIG_DIR/trust.d"
    cp samples/keys/sample-2026.pub "$SVCD_CONFIG_DIR/trust.d/sample-2026.pub"

    target/release/plugin install target/realtime.spk
    target/release/plugin start realtime
    # Open http://127.0.0.1:8787/plugin-ui/realtime/

If a system svcd is already running, put the public key in its real
`<config>/trust.d/` instead of the local override.

The sample agent uses a python3 shebang; run it on Linux or macOS (or wrap it
in a platform launcher for Windows).

## What to look for

- SSE panel: a tick roughly every two seconds; closing and reopening the
  EventSource reconnects with Last-Event-ID and the server replays events from
  its bounded ring.
- WebSocket panel: ticks and echo responses share one connection; the
  reconnect button exercises the close path.
- event: reset means the requested Last-Event-ID fell out of the replay
  window; the page should refetch state with tools/call.
