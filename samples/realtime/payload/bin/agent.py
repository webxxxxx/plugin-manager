#!/usr/bin/env python3
"""Minimal realtime UI sample agent.

Writes one JSON-RPC notification every two seconds and answers svcd.ping /
tools/call on stdin. Keep stdout protocol-only; use stderr for human logs.
"""
import json
import sys
import threading
import time

lock = threading.Lock()
tick = 0


def send(message):
    line = json.dumps(message, separators=(",", ":"), ensure_ascii=False)
    with lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def emit_ticks():
    global tick
    while True:
        time.sleep(2)
        tick += 1
        send(
            {
                "jsonrpc": "2.0",
                "method": "notifications/message",
                "params": {
                    "level": "info",
                    "data": json.dumps({"tick": tick, "ts": int(time.time())}),
                },
            }
        )


threading.Thread(target=emit_ticks, daemon=True).start()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        request = json.loads(line)
    except json.JSONDecodeError:
        continue

    request_id = request.get("id")
    method = request.get("method")
    if method == "svcd.ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
    elif method == "tools/call":
        params = request.get("params") or {}
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": json.dumps(
                                {
                                    "capability": params.get("name"),
                                    "arguments": params.get("arguments") or {},
                                },
                                ensure_ascii=False,
                            ),
                        }
                    ],
                    "isError": False,
                },
            }
        )
    elif request_id is not None:
        send(
            {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": "method not found"},
            }
        )
