#!/usr/bin/env python3
"""20-plugin reactor regression with 1000 real client threads.

Unlike ``reactor_scale.py`` (asyncio clients), this harness creates a pool of
1000 OS threads so the client side matches the requested thread count. It also
samples the daemon's CPU, RSS/VM, thread count and process tree.

Usage (WSL/Linux):
  python3 tools/perf/reactor_scale_threads.py \
    --binary target/release/svcd --work /tmp/svcd-reactor-1000 \
    --plugins 20 --threads 1000
"""

import argparse
import base64
import concurrent.futures
import hashlib
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import reactor_scale  # noqa: E402

PASSWORD = reactor_scale.PASSWORD


def process_ticks(pid):
    try:
        text = pathlib.Path(f"/proc/{pid}/stat").read_text()
        rest = text.rsplit(")", 1)[1].split()
        return int(rest[11]) + int(rest[12])
    except (OSError, IndexError, ValueError):
        return 0


def process_children(pid):
    try:
        text = pathlib.Path(f"/proc/{pid}/task/{pid}/children").read_text()
        return [int(value) for value in text.split()]
    except (OSError, ValueError):
        return []


def descendants(pid):
    found = []
    pending = process_children(pid)
    while pending:
        child = pending.pop()
        if child in found:
            continue
        found.append(child)
        pending.extend(process_children(child))
    return found


class Metrics:
    def __init__(self, pid):
        self.pid = pid
        self.lock = threading.Lock()
        self.stop_event = threading.Event()
        self.started = time.monotonic()
        self.cpu_start = process_ticks(pid)
        self.max_rss = 0
        self.max_vm = 0
        self.max_threads = 0
        self.max_processes = 0
        self.max_cpu = 0.0
        self.last_ticks = self.cpu_start
        self.last_at = self.started
        self.running = True
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _read(self):
        rss = vm = threads = 0
        try:
            for line in pathlib.Path(f"/proc/{self.pid}/status").read_text().splitlines():
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1])
                elif line.startswith("VmSize:"):
                    vm = int(line.split()[1])
                elif line.startswith("Threads:"):
                    threads = int(line.split()[1])
        except OSError:
            pass
        return rss, vm, threads, 1 + len(descendants(self.pid))

    def _run(self):
        while not self.stop_event.is_set():
            now = time.monotonic()
            ticks = process_ticks(self.pid)
            with self.lock:
                rss, vm, threads, processes = self._read()
                self.max_rss = max(self.max_rss, rss)
                self.max_vm = max(self.max_vm, vm)
                self.max_threads = max(self.max_threads, threads)
                self.max_processes = max(self.max_processes, processes)
                elapsed = now - self.last_at
                if elapsed > 0:
                    self.max_cpu = max(
                        self.max_cpu,
                        (ticks - self.last_ticks) / os.sysconf("SC_CLK_TCK") / elapsed * 100,
                    )
                self.last_ticks = ticks
                self.last_at = now
            time.sleep(0.05)

    def reset(self):
        with self.lock:
            self.started = time.monotonic()
            self.cpu_start = process_ticks(self.pid)
            self.last_ticks = self.cpu_start
            self.last_at = self.started
            self.max_rss = 0
            self.max_vm = 0
            self.max_threads = 0
            self.max_processes = 0
            self.max_cpu = 0.0

    def summary(self):
        with self.lock:
            elapsed = max(time.monotonic() - self.started, 0.001)
            total_cpu = (process_ticks(self.pid) - self.cpu_start) / os.sysconf("SC_CLK_TCK")
            rss = self.max_rss
            vm = self.max_vm
            threads = self.max_threads
            processes = self.max_processes
            max_cpu = self.max_cpu
        return {
            "elapsed_sec": round(elapsed, 3),
            "cpu_avg_percent": round(total_cpu / elapsed * 100, 1),
            "cpu_peak_percent": round(max_cpu, 1),
            "max_rss_kb": rss,
            "max_vm_kb": vm,
            "max_threads": threads,
            "max_processes": processes,
        }

    def stop(self):
        self.stop_event.set()
        self.thread.join(timeout=1)


def http_get(address, path, headers=None):
    host, port = address.rsplit(":", 1)
    import http.client
    conn = http.client.HTTPConnection(host, int(port), timeout=60)
    try:
        conn.request("GET", path, headers=headers or {"Connection": "close"})
        response = conn.getresponse()
        response.read()
        return response.status
    finally:
        conn.close()


def http_mcp(address, plugin, index):
    import http.client
    host, port = address.rsplit(":", 1)
    body = json.dumps({
        "jsonrpc": "2.0", "id": index, "method": "tools/call",
        "params": {
            "name": f"{plugin}.echo", "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    }).encode()
    conn = http.client.HTTPConnection(host, int(port), timeout=60)
    try:
        conn.request(
            "POST", "/mcp", body,
            {
                "Authorization": f"Bearer {PASSWORD}",
                "Content-Type": "application/json",
                "Accept": "application/json",
                "MCP-Protocol-Version": "2026-07-28",
                "Mcp-Method": "tools/call",
                "Mcp-Name": f"{plugin}.echo",
                "Connection": "close",
            },
        )
        response = conn.getresponse()
        payload = response.read()
        if response.status != 200:
            return response.status, False
        value = json.loads(payload)
        return 200, "result" in value and not value["result"].get("isError", False)
    finally:
        conn.close()


def read_headers(sock):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    if not data:
        return 0
    line = data.split(b"\r\n", 1)[0].decode("latin1")
    parts = line.split()
    return int(parts[1]) if len(parts) > 1 else 0


def sse_connect(address, plugin, hold):
    host, port = address.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)), timeout=60)
    try:
        sock.sendall(
            (
                f"GET /plugin-ui/{plugin}/events HTTP/1.1\r\n"
                f"Host: {address}\r\n"
                "Accept: text/event-stream\r\n"
                "Connection: keep-alive\r\n\r\n"
            ).encode()
        )
        status = read_headers(sock)
        if status == 200:
            time.sleep(hold)
        return status
    finally:
        sock.close()


def ws_connect(address, plugin, hold):
    host, port = address.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)), timeout=60)
    try:
        key = base64.b64encode(os.urandom(16)).decode()
        sock.sendall(
            (
                f"GET /plugin-ui/{plugin}/ws HTTP/1.1\r\n"
                f"Host: {address}\r\n"
                "Upgrade: websocket\r\n"
                "Connection: Upgrade\r\n"
                "Sec-WebSocket-Version: 13\r\n"
                f"Sec-WebSocket-Key: {key}\r\n\r\n"
            ).encode()
        )
        status = read_headers(sock)
        if status == 101:
            time.sleep(hold)
        return status
    finally:
        sock.close()


def run_phase(metrics, fn, tasks):
    metrics.reset()
    started = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(tasks)) as pool:
        results = list(pool.map(fn, tasks))
    elapsed = time.perf_counter() - started
    summary = metrics.summary()
    summary["elapsed_sec"] = round(elapsed, 3)
    return results, summary


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/svcd")
    parser.add_argument("--work", default="/tmp/svcd-reactor-1000")
    parser.add_argument("--plugins", type=int, default=20)
    parser.add_argument("--threads", type=int, default=1000)
    parser.add_argument("--hold", type=float, default=2.0)
    parser.add_argument("--key", default="samples/keys/sample-2026.key")
    parser.add_argument("--pub", default="samples/keys/sample-2026.pub")
    args = parser.parse_args()

    threading.stack_size(256 * 1024)
    root = pathlib.Path(args.work).resolve()
    binary = pathlib.Path(args.binary).resolve()
    key = pathlib.Path(args.key).resolve()
    pub = pathlib.Path(args.pub).resolve()
    env = reactor_scale.setup(root, binary, key, pub)
    fixture = reactor_scale.make_fixture(root)
    daemon = subprocess.Popen(
        [str(root / "bin" / "svcd"), "run", "--foreground"],
        env=env, stdout=(root / "daemon.log").open("w"),
        stderr=subprocess.STDOUT, start_new_session=True,
    )
    metrics = Metrics(daemon.pid)
    try:
        address = reactor_scale.wait_for_http(root / "config", daemon)
        reactor_scale.install_plugins(root, env, fixture, args.plugins, key)
        time.sleep(0.5)
        per_plugin = max(1, args.threads // args.plugins)

        http_tasks = [
            (f"p{i % args.plugins}", i) for i in range(args.threads)
        ]
        http_results, http_metrics = run_phase(
            metrics,
            lambda task: http_get(address, f"/plugin-ui/{task[0]}/"),
            http_tasks,
        )

        mcp_tasks = [
            (f"p{i % args.plugins}", i) for i in range(args.threads)
        ]
        mcp_results, mcp_metrics = run_phase(
            metrics,
            lambda task: http_mcp(address, *task),
            mcp_tasks,
        )

        stream_tasks = [
            (f"p{i % args.plugins}",) for i in range(args.threads)
        ]
        sse_results, sse_metrics = run_phase(
            metrics,
            lambda task: sse_connect(address, task[0], args.hold),
            stream_tasks,
        )
        ws_results, ws_metrics = run_phase(
            metrics,
            lambda task: ws_connect(address, task[0], args.hold),
            stream_tasks,
        )

        result = {
            "plugins": args.plugins,
            "client_threads": args.threads,
            "concurrency_per_plugin": per_plugin,
            "http": {
                "requests": len(http_results),
                "ok": sum(1 for status in http_results if status == 200),
                "other": sum(1 for status in http_results if status != 200),
                "metrics": http_metrics,
            },
            "mcp": {
                "requests": len(mcp_results),
                "ok": sum(1 for _, ok in mcp_results if ok),
                "other": sum(1 for _, ok in mcp_results if not ok),
                "metrics": mcp_metrics,
            },
            "sse": {
                "connections": len(sse_results),
                "accepted": sum(1 for status in sse_results if status == 200),
                "rejected": sum(1 for status in sse_results if status != 200),
                "metrics": sse_metrics,
            },
            "ws": {
                "connections": len(ws_results),
                "accepted": sum(1 for status in ws_results if status == 101),
                "rejected": sum(1 for status in ws_results if status != 101),
                "metrics": ws_metrics,
            },
        }
        print(json.dumps(result, indent=2))
        (root / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    finally:
        metrics.stop()
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
