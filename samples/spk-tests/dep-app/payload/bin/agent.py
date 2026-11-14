#!/usr/bin/env python3
"""dep-app: declares `dependencies: ["dep-base"]` in its manifest.

The dependency is resolved by svcd at start time, so this agent can report the
contract it was started under even though it cannot see the other service.
"""
import json
import os
import sys
import threading
import time

write_lock = threading.Lock()
started = time.time()


def send(message):
    line = json.dumps(message, separators=(",", ":"), ensure_ascii=False)
    with write_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


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
        send({"jsonrpc": "2.0", "id": request_id, "result": {"service": "dep-app"}})
        return
    if method != "tools/call":
        fail(request_id, -32601, "method not found")
        return

    params = request.get("params") or {}
    capability = params.get("name")
    if capability == "report":
        result(
            request_id,
            json.dumps(
                {
                    "service": "dep-app",
                    "pid": os.getpid(),
                    "uptime_s": round(time.time() - started, 3),
                    "depends_on": ["dep-base"],
                    "note": "svcd resolves dependencies before starting this service",
                }
            ),
        )
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
