#!/usr/bin/env python3
"""ws-panel: emits one notification per second for the WebSocket UI.

The UI opens `/plugin-ui/ws-panel/ws`; svcd mirrors every `notifications/*`
frame written here to all of that plugin's UI subscribers. UI requests arrive
as `tools/call` frames on the same socket.
"""
import json
import os
import sys
import threading
import time

write_lock = threading.Lock()
started = time.time()
tick = 0


def send(message):
    line = json.dumps(message, separators=(",", ":"), ensure_ascii=False)
    with write_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


def notify(payload):
    send(
        {
            "jsonrpc": "2.0",
            "method": "notifications/message",
            "params": {"level": "info", "data": payload},
        }
    )


def result(request_id, text):
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"content": [{"type": "text", "text": text}], "isError": False},
        }
    )


def fail(request_id, code, message):
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def ticker():
    global tick
    while True:
        time.sleep(1)
        with write_lock:
            tick += 1
            current = tick
        notify(
            {
                "kind": "tick",
                "tick": current,
                "ts": int(time.time() * 1000),
                "uptime_s": round(time.time() - started, 1),
            }
        )


threading.Thread(target=ticker, daemon=True).start()


def handle(request):
    request_id = request.get("id")
    method = request.get("method")

    if method == "svcd.ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        return
    if method == "svcd.shutdown":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        sys.exit(0)
    if method == "svcd.status":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"service": "ws-panel"}})
        return
    if method != "tools/call":
        fail(request_id, -32601, "method not found")
        return

    params = request.get("params") or {}
    capability = params.get("name")
    arguments = params.get("arguments") or {}

    if capability == "status":
        result(
            request_id,
            json.dumps(
                {
                    "service": "ws-panel",
                    "pid": os.getpid(),
                    "tick": tick,
                    "uptime_s": round(time.time() - started, 1),
                }
            ),
        )
    elif capability == "emit":
        text = arguments.get("text")
        if not isinstance(text, str) or not text:
            fail(request_id, -32602, "emit requires a non-empty string argument: text")
            return
        notify(
            {
                "kind": "emit",
                "text": text,
                "tick": tick,
                "ts": int(time.time() * 1000),
            }
        )
        result(request_id, "emit queued: " + text)
    else:
        fail(request_id, -32602, "unknown capability: %s" % capability)


for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        message = json.loads(raw)
    except json.JSONDecodeError:
        continue
    if isinstance(message, dict):
        handle(message)
