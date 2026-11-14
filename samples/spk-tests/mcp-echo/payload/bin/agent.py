#!/usr/bin/env python3
"""mcp-echo: a tiny capability server for exercising the MCP surface.

The daemon exposes `echo`, `add` and `status` as `mcp-echo.echo` and friends on
`POST /mcp` and on the stdio `mcp` bridge. No state outlives the process.
"""
import json
import os
import sys
import threading
import time

write_lock = threading.Lock()
calls = 0
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


def tool_error(request_id, text):
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {"content": [{"type": "text", "text": text}], "isError": True},
        }
    )


def fail(request_id, code, message):
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def handle(request):
    global calls
    request_id = request.get("id")
    method = request.get("method")

    if method == "svcd.ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        return
    if method == "svcd.shutdown":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
        sys.exit(0)
    if method == "svcd.status":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"service": "mcp-echo"}})
        return
    if method != "tools/call":
        fail(request_id, -32601, "method not found")
        return

    params = request.get("params") or {}
    capability = params.get("name")
    arguments = params.get("arguments") or {}
    calls += 1

    if capability == "echo":
        text = arguments.get("text")
        if not isinstance(text, str):
            tool_error(request_id, "echo requires a string argument: text")
            return
        result(request_id, "mcp-echo: " + text)
    elif capability == "add":
        try:
            total = float(arguments["a"]) + float(arguments["b"])
        except (KeyError, TypeError, ValueError):
            tool_error(request_id, "add requires numeric arguments: a, b")
            return
        text = str(int(total)) if total.is_integer() else str(total)
        result(request_id, text)
    elif capability == "status":
        result(
            request_id,
            json.dumps(
                {
                    "service": "mcp-echo",
                    "pid": os.getpid(),
                    "uptime_s": round(time.time() - started, 3),
                    "calls": calls,
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
