#!/usr/bin/env python3
"""20-plugin reactor regression for HTTP / SSE / WebSocket / MCP.

Runs only on Unix (the reactor path). It builds a minimal shell-plugin fixture,
installs and starts N copies, then drives concurrent HTTP, MCP, SSE and WS
traffic while sampling the daemon's thread count.

Usage (WSL/Linux):
  python3 tools/perf/reactor_scale.py \
    --binary target/release/svcd \
    --work /tmp/svcd-reactor-20 \
    --plugins 20 --concurrency 100
"""

import argparse
import asyncio
import base64
import hashlib
import json
import os
import pathlib
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time

PASSWORD = "reactor-test-password"
KEY_ID = "sample-2026"


def run(cmd, env, timeout=30):
    return subprocess.run(
        cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, timeout=timeout,
    )


def wait_for_http(config, daemon, timeout=15):
    port_file = config / "http.port"
    deadline = time.time() + timeout
    while time.time() < deadline:
        if daemon.poll() is not None:
            raise RuntimeError("daemon exited during startup")
        if port_file.is_file():
            address = port_file.read_text().strip()
            host, port = address.rsplit(":", 1)
            try:
                with socket.create_connection((host, int(port)), timeout=0.2):
                    return address
            except OSError:
                pass
        time.sleep(0.05)
    raise RuntimeError("daemon HTTP port did not become ready")


def make_fixture(root):
    fixture = root / "fixture"
    (fixture / "payload" / "bin").mkdir(parents=True)
    (fixture / "payload" / "web").mkdir(parents=True)
    agent = r"""#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=1; fi
  case "$line" in
    *svcd.ping*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      ;;
    *'"name":"echo"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id"
      ;;
    *'"name":"emit"'*)
      printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"tick"}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}\n' "$id"
      ;;
  esac
done
"""
    (fixture / "payload" / "bin" / "agent.sh").write_text(agent)
    os.chmod(fixture / "payload" / "bin" / "agent.sh", 0o755)
    (fixture / "payload" / "web" / "index.html").write_text(
        "<!doctype html><html><body>perf</body></html>\n"
    )
    manifest = {
        "schema": 1,
        "protocol": 1,
        "name": "perf",
        "version": "1.0.0",
        "desc": "reactor scale fixture",
        "platform": "x86_64-unknown-linux-gnu",
        "exec": {"path": "payload/bin/agent.sh", "ready": "notify"},
        "capabilities": [
            {"name": "echo", "desc": "echo", "inputSchema": {"type": "object"}},
            {"name": "emit", "desc": "emit", "inputSchema": {"type": "object"}},
        ],
        "ui": {"type": "web", "entry": "index.html"},
    }
    (fixture / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    return fixture


def setup(root, binary, key, pub):
    if root.exists():
        shutil.rmtree(root)
    for name in ("bin", "config", "data", "logs"):
        (root / name).mkdir(parents=True)
    (root / "config" / "trust.d").mkdir()
    for name in ("svcd", "svc", "plugin", "mcp"):
        target = root / "bin" / name
        shutil.copyfile(binary, target)
        target.chmod(0o755)
    shutil.copyfile(pub, root / "config" / "trust.d" / f"{KEY_ID}.pub")
    salt = os.urandom(16)
    digest = hashlib.pbkdf2_hmac("sha256", PASSWORD.encode(), salt, 10000)
    encoded = (
        "pbkdf2-sha256$10000$"
        + base64.urlsafe_b64encode(salt).decode().rstrip("=")
        + "$"
        + base64.urlsafe_b64encode(digest).decode().rstrip("=")
    )
    (root / "config" / "svcd.conf").write_text(
        f"http_port = 0\nauth_passwd = {encoded}\n"
    )
    env = os.environ.copy()
    env.update({
        "SVCD_CONFIG_DIR": str(root / "config"),
        "SVCD_DATA_DIR": str(root / "data"),
        "SVCD_LOG_DIR": str(root / "logs"),
        "SVCD_TRUST_DIR": str(root / "config" / "trust.d"),
        "SVCD_HTTP_PORT": "0",
        "SVCD_TEST_UNPRIVILEGED": "1",
    })
    return env


def install_plugins(root, env, fixture, count, key):
    plugin = root / "bin" / "plugin"
    svc = root / "bin" / "svc"
    for index in range(count):
        plugin_id = f"p{index}"
        spk = root / f"{plugin_id}.spk"
        bundled = run([
            str(plugin), "bundle", str(fixture),
            "--id", plugin_id, "--key", str(key),
            "--key-id", KEY_ID, "-o", str(spk),
        ], env)
        if bundled.returncode != 0:
            raise RuntimeError(f"bundle {plugin_id}: {bundled.stderr}")
        installed = run([str(plugin), "install", str(spk)], env)
        if installed.returncode != 0:
            raise RuntimeError(f"install {plugin_id}: {installed.stderr}")
        started = run([str(svc), "start", plugin_id], env, timeout=40)
        if started.returncode != 0:
            raise RuntimeError(f"start {plugin_id}: {started.stderr}")


class ThreadSampler(threading.Thread):
    def __init__(self, pid):
        super().__init__(daemon=True)
        self.pid = pid
        self.stop_event = threading.Event()
        self.maximum = 0

    def run(self):
        while not self.stop_event.is_set():
            try:
                text = pathlib.Path(f"/proc/{self.pid}/status").read_text()
                for line in text.splitlines():
                    if line.startswith("Threads:"):
                        self.maximum = max(self.maximum, int(line.split()[1]))
                        break
            except OSError:
                break
            time.sleep(0.02)

    def stop(self):
        self.stop_event.set()
        self.join(timeout=1)


async def read_headers(reader):
    head = await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), 60)
    lines = head.decode("latin1").split("\r\n")
    status = int(lines[0].split()[1])
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            key, value = line.split(":", 1)
            headers[key.strip().lower()] = value.strip()
    return status, headers


async def http_request(address, method, path, headers, body=b""):
    host, port = address.rsplit(":", 1)
    reader, writer = await asyncio.open_connection(host, int(port))
    head = [f"{method} {path} HTTP/1.1", f"Host: {address}",
            "Connection: close"]
    if body:
        head.append(f"Content-Length: {len(body)}")
    for key, value in headers.items():
        head.append(f"{key}: {value}")
    payload = ("\r\n".join(head) + "\r\n\r\n").encode() + body
    writer.write(payload)
    await writer.drain()
    status, response_headers = await read_headers(reader)
    length = int(response_headers.get("content-length", "0"))
    response_body = await reader.readexactly(length) if length else b""
    writer.close()
    await writer.wait_closed()
    return status, response_body


async def stream_request(address, path, headers, hold_seconds):
    host, port = address.rsplit(":", 1)
    reader, writer = await asyncio.open_connection(host, int(port))
    head = [f"GET {path} HTTP/1.1", f"Host: {address}",
            "Connection: keep-alive", "Accept: text/event-stream"]
    for key, value in headers.items():
        head.append(f"{key}: {value}")
    writer.write(("\r\n".join(head) + "\r\n\r\n").encode())
    await writer.drain()
    status, _ = await read_headers(reader)
    if status == 200:
        await asyncio.sleep(hold_seconds)
    writer.close()
    await writer.wait_closed()
    return status


async def ws_connect(address, path, hold_seconds):
    host, port = address.rsplit(":", 1)
    reader, writer = await asyncio.open_connection(host, int(port))
    key = base64.b64encode(os.urandom(16)).decode()
    head = [
        f"GET {path} HTTP/1.1",
        f"Host: {address}",
        "Upgrade: websocket",
        "Connection: Upgrade",
        "Sec-WebSocket-Version: 13",
        f"Sec-WebSocket-Key: {key}",
    ]
    writer.write(("\r\n".join(head) + "\r\n\r\n").encode())
    await writer.drain()
    status, _ = await read_headers(reader)
    if status == 101:
        await asyncio.sleep(hold_seconds)
    writer.close()
    await writer.wait_closed()
    return status


async def phase_http(address, count, concurrency):
    async def one(index):
        plugin = index % count
        return await http_request(
            address, "GET", f"/plugin-ui/p{plugin}/",
            {"Accept": "text/html"},
        )

    started = time.perf_counter()
    results = await asyncio.gather(*(one(i) for i in range(count * concurrency)))
    return {
        "requests": len(results),
        "ok": sum(1 for status, _ in results if status == 200),
        "other": sum(1 for status, _ in results if status != 200),
        "seconds": round(time.perf_counter() - started, 3),
    }


async def phase_mcp(address, per_plugin, total_plugins):
    async def one(index):
        plugin = index % total_plugins
        body = json.dumps({
            "jsonrpc": "2.0", "id": index, "method": "tools/call",
            "params": {
                "name": f"p{plugin}.echo", "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            },
        }).encode()
        status, payload = await http_request(
            address, "POST", "/mcp",
            {
                "Authorization": f"Bearer {PASSWORD}",
                "Content-Type": "application/json",
                "Accept": "application/json",
                "MCP-Protocol-Version": "2026-07-28",
                "Mcp-Method": "tools/call",
                "Mcp-Name": f"p{plugin}.echo",
            },
            body,
        )
        ok = False
        busy = False
        if status == 200:
            try:
                value = json.loads(payload)
                ok = "result" in value and not value["result"].get("isError", False)
                busy = "error" in value and value["error"].get("code") == -32000
            except Exception:
                pass
        return status, ok, busy

    started = time.perf_counter()
    results = await asyncio.gather(
        *(one(i) for i in range(per_plugin * total_plugins))
    )
    return {
        "requests": len(results),
        "ok": sum(1 for _, ok, _ in results if ok),
        "busy": sum(1 for _, _, busy in results if busy),
        "other": sum(1 for status, ok, busy in results if status != 200 or not ok and not busy),
        "seconds": round(time.perf_counter() - started, 3),
    }


async def phase_streams(address, per_plugin, total_plugins, kind, hold):
    async def one(index):
        plugin = index % total_plugins
        if kind == "sse":
            return await stream_request(
                address, f"/plugin-ui/p{plugin}/events", {}, hold
            )
        return await ws_connect(address, f"/plugin-ui/p{plugin}/ws", hold)

    started = time.perf_counter()
    results = await asyncio.gather(
        *(one(i) for i in range(per_plugin * total_plugins))
    )
    return {
        "connections": len(results),
        "accepted": sum(1 for status in results if status in (101, 200)),
        "rejected": sum(1 for status in results if status not in (101, 200)),
        "seconds": round(time.perf_counter() - started, 3),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/svcd")
    parser.add_argument("--work", default="/tmp/svcd-reactor-scale")
    parser.add_argument("--plugins", type=int, default=20)
    parser.add_argument("--concurrency", type=int, default=100)
    parser.add_argument("--key", default="samples/keys/sample-2026.key")
    parser.add_argument("--pub", default="samples/keys/sample-2026.pub")
    parser.add_argument("--hold", type=float, default=2.0)
    args = parser.parse_args()

    root = pathlib.Path(args.work).resolve()
    binary = pathlib.Path(args.binary).resolve()
    key = pathlib.Path(args.key).resolve()
    pub = pathlib.Path(args.pub).resolve()
    env = setup(root, binary, key, pub)
    fixture = make_fixture(root)

    daemon = subprocess.Popen(
        [str(root / "bin" / "svcd"), "run", "--foreground"],
        env=env, stdout=(root / "daemon.log").open("w"),
        stderr=subprocess.STDOUT, start_new_session=True,
    )
    sampler = ThreadSampler(daemon.pid)
    sampler.start()
    try:
        address = wait_for_http(root / "config", daemon)
        install_plugins(root, env, fixture, args.plugins, key)
        time.sleep(0.5)

        http = asyncio.run(phase_http(address, args.plugins, args.concurrency))
        mcp = asyncio.run(phase_mcp(address, args.concurrency, args.plugins))
        sse = asyncio.run(phase_streams(
            address, args.concurrency, args.plugins, "sse", args.hold
        ))
        ws = asyncio.run(phase_streams(
            address, args.concurrency, args.plugins, "ws", args.hold
        ))
        sampler.stop()
        result = {
            "plugins": args.plugins,
            "concurrency_per_plugin": args.concurrency,
            "daemon_max_threads": sampler.maximum,
            "http": http,
            "mcp": mcp,
            "sse": sse,
            "ws": ws,
        }
        print(json.dumps(result, indent=2))
        (root / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    finally:
        sampler.stop()
        try:
            subprocess.run(
                [str(root / "bin" / "svcd"), "shutdown"], env=env,
                timeout=5, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
        except Exception:
            daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except Exception:
            daemon.kill()


if __name__ == "__main__":
    main()
