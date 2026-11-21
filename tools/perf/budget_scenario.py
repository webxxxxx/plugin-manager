#!/usr/bin/env python3
"""Memory-budget scenario: N plugins under mixed traffic for a fixed duration.

Each plugin is driven through all five surfaces the issue names:

  invoke   MCP tools/call (POST /mcp)
  sse      /plugin-ui/<id>/events, triggered by an invoke
  ws       /plugin-ui/<id>/ws WebSocket tools/call
  upload   POST /api/v1/blobs
  download GET  /api/v1/blobs/<id>

The daemon runs with an explicit mem_budget_bytes so the derived limits can be
compared with the traffic that actually fits.  Daemon and process-tree RSS/CPU
are sampled throughout; the run ends with an oversubscription probe that opens
more streams than the budget can admit and checks that the refusals are the
explicit 429/503 kind rather than a crash.

Cross-platform and stdlib-only: /proc on Linux, psapi/Toolhelp32 on Windows.

Usage:
  python3 tools/perf/budget_scenario.py \
    --binary target/release/svcd --budget 50mb --plugins 10 \
    --duration 60 --concurrency 4
"""

import argparse
import base64
import hashlib
import http.client
import json
import os
import platform as hostinfo
import re
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

MIB = 1024 * 1024
WIN = os.name == "nt"
EXE = ".exe" if WIN else ""
OPS = ("invoke", "sse", "ws", "upload", "download")


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------

def log(message):
    print("[budget] " + message, flush=True)


def parse_size(text):
    match = re.fullmatch(r"\s*([0-9]+(?:\.[0-9]+)?)\s*([a-zA-Z]*)\s*", str(text))
    if not match:
        raise argparse.ArgumentTypeError("not a size: " + text)
    value = float(match.group(1))
    unit = match.group(2).lower()
    scale = {
        "": 1, "b": 1,
        "k": 1024, "kb": 1024, "kib": 1024,
        "m": MIB, "mb": MIB, "mib": MIB,
        "g": 1024 * MIB, "gb": 1024 * MIB, "gib": 1024 * MIB,
    }.get(unit)
    if scale is None:
        raise argparse.ArgumentTypeError("unknown size unit: " + unit)
    return int(value * scale)


def http_request(address, method, path, headers=None, body=None, timeout=10.0):
    host, port = address.rsplit(":", 1)
    connection = http.client.HTTPConnection(host, int(port), timeout=timeout)
    request_headers = dict(headers or {})
    if body is not None:
        request_headers.setdefault("Content-Length", str(len(body)))
    try:
        connection.request(method, path, body=body, headers=request_headers)
        response = connection.getresponse()
        data = response.read()
        return response.status, {k.lower(): v for k, v in response.getheaders()}, data
    finally:
        connection.close()


def read_json(data):
    try:
        return json.loads(data.decode("utf-8"))
    except Exception:
        return None


def wait_for_http(root, daemon, timeout=30.0):
    # ADR-0023 moved the run artifacts under the data root; older drivers still
    # look in config/. Check both so this script works on either layout.
    candidates = [root / "data" / "run" / "http.port", root / "config" / "http.port"]
    deadline = time.time() + timeout
    while time.time() < deadline:
        if daemon.poll() is not None:
            raise RuntimeError("daemon exited during startup (code %s)" % daemon.returncode)
        for port_file in candidates:
            if port_file.is_file():
                address = port_file.read_text().strip()
                host, port = address.rsplit(":", 1)
                try:
                    with socket.create_connection((host, int(port)), timeout=0.2):
                        return address
                except OSError:
                    pass
        time.sleep(0.05)
    raise RuntimeError("daemon HTTP never became ready")


def startup_report(root, timeout=10.0):
    path = root / "data" / "run" / "resources" / "startup.json"
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.is_file():
            try:
                return json.loads(path.read_text(encoding="utf-8"))
            except Exception:
                pass
        time.sleep(0.05)
    return None


# --------------------------------------------------------------------------
# process sampling
# --------------------------------------------------------------------------

if WIN:
    import ctypes
    from ctypes import wintypes

    class _ProcessMemoryCounters(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    class _ProcessEntry32(ctypes.Structure):
        _fields_ = [
            ("dwSize", wintypes.DWORD),
            ("cntUsage", wintypes.DWORD),
            ("th32ProcessID", wintypes.DWORD),
            ("th32DefaultHeapID", ctypes.c_void_p),
            ("th32ModuleID", wintypes.DWORD),
            ("cntThreads", wintypes.DWORD),
            ("th32ParentProcessID", wintypes.DWORD),
            ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", wintypes.DWORD),
            ("szExeFile", ctypes.c_char * 260),
        ]

    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _psapi = ctypes.WinDLL("psapi", use_last_error=True)

    _kernel32.OpenProcess.restype = wintypes.HANDLE
    _kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    _psapi.GetProcessMemoryInfo.restype = wintypes.BOOL
    _psapi.GetProcessMemoryInfo.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(_ProcessMemoryCounters), wintypes.DWORD,
    ]
    _kernel32.GetProcessTimes.restype = wintypes.BOOL
    _kernel32.GetProcessTimes.argtypes = [
        wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME),
        ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME),
    ]
    _kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    _kernel32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    _kernel32.Process32First.restype = wintypes.BOOL
    _kernel32.Process32First.argtypes = [wintypes.HANDLE, ctypes.POINTER(_ProcessEntry32)]
    _kernel32.Process32Next.restype = wintypes.BOOL
    _kernel32.Process32Next.argtypes = [wintypes.HANDLE, ctypes.POINTER(_ProcessEntry32)]
    _kernel32.CloseHandle.argtypes = [wintypes.HANDLE]

    TH32CS_SNAPPROCESS = 0x00000002
    QUERY_ACCESS = 0x1000 | 0x0400

    def _process_info(pid):
        handle = _kernel32.OpenProcess(QUERY_ACCESS, False, pid)
        if not handle:
            return None
        try:
            counters = _ProcessMemoryCounters()
            counters.cb = ctypes.sizeof(counters)
            if not _psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                return None
            created, exited, kernel, user = (
                wintypes.FILETIME(), wintypes.FILETIME(),
                wintypes.FILETIME(), wintypes.FILETIME(),
            )
            allowed = _kernel32.GetProcessTimes(
                handle, ctypes.byref(created), ctypes.byref(exited),
                ctypes.byref(kernel), ctypes.byref(user),
            )
            cpu = 0.0
            if allowed:
                ticks = (kernel.dwHighDateTime << 32 | kernel.dwLowDateTime) + (
                    user.dwHighDateTime << 32 | user.dwLowDateTime
                )
                cpu = ticks / 10_000_000.0
            return {"rss": int(counters.WorkingSetSize), "private": int(counters.PagefileUsage), "cpu": cpu}
        finally:
            _kernel32.CloseHandle(handle)

    def _process_table():
        snapshot = _kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
        if snapshot in (0, ctypes.c_void_p(-1).value):
            return {}
        table = {}
        try:
            entry = _ProcessEntry32()
            entry.dwSize = ctypes.sizeof(entry)
            if not _kernel32.Process32First(snapshot, ctypes.byref(entry)):
                return {}
            while True:
                table[int(entry.th32ProcessID)] = {
                    "ppid": int(entry.th32ParentProcessID),
                    "threads": int(entry.cntThreads),
                }
                if not _kernel32.Process32Next(snapshot, ctypes.byref(entry)):
                    break
        finally:
            _kernel32.CloseHandle(snapshot)
        return table

    def process_tree(pid):
        if not pid:
            return None
        table = _process_table()
        if not table:
            return None
        children = {}
        for child, info in table.items():
            children.setdefault(info["ppid"], []).append(child)
        seen = []
        pending = [pid]
        while pending:
            current = pending.pop()
            if current in seen:
                continue
            seen.append(current)
            pending.extend(children.get(current, []))
        rss = private = cpu = 0
        threads = 0
        for current in seen:
            info = _process_info(current)
            if info:
                rss += info["rss"]
                private += info["private"]
                cpu += info["cpu"]
            threads += table.get(current, {}).get("threads", 0)
        return {"processes": len(seen), "rss": rss, "private": private, "cpu": cpu, "threads": threads}

else:
    CLK_TCK = os.sysconf("SC_CLK_TCK")

    def _read_status(pid):
        try:
            text = Path("/proc/%d/status" % pid).read_text()
        except OSError:
            return None
        values = {}
        for line in text.splitlines():
            if line.startswith(("VmRSS:", "Threads:")):
                key, value = line.split(":", 1)
                values[key] = int(value.split()[0])
        return {
            "rss": values.get("VmRSS", 0) * 1024,
            "threads": values.get("Threads", 0),
            "pss": _read_pss(pid),
        }

    def _read_pss(pid):
        """Proportional set size: shared pages split across their owners.

        Summed RSS counts one libc page once per plugin process, which is why a
        plugin count read off RSS looks far more expensive than it is.
        """
        try:
            text = Path("/proc/%d/smaps_rollup" % pid).read_text()
        except OSError:
            return 0
        for line in text.splitlines():
            if line.startswith("Pss:"):
                return int(line.split()[1]) * 1024
        return 0

    def _read_stat(pid):
        try:
            text = Path("/proc/%d/stat" % pid).read_text()
        except OSError:
            return None
        close = text.rfind(")")
        fields = text[close + 2:].split()
        if len(fields) < 13:
            return None
        return {
            "ppid": int(fields[1]),
            "cpu": (int(fields[11]) + int(fields[12])) / CLK_TCK,
        }

    def process_tree(pid):
        if not pid:
            return None
        table = {}
        for name in os.listdir("/proc"):
            if not name.isdigit():
                continue
            stat = _read_stat(int(name))
            if stat is None:
                continue
            status = _read_status(int(name))
            if status is None:
                continue
            table[int(name)] = dict(stat, **status)
        seen = []
        pending = [pid]
        while pending:
            current = pending.pop()
            if current in seen or current not in table:
                continue
            seen.append(current)
            pending.extend([
                child for child, info in table.items() if info["ppid"] == current
            ])
        rss = private = pss = cpu = 0
        threads = 0
        for current in seen:
            info = table[current]
            rss += info["rss"]
            private += info["rss"]
            pss += info.get("pss", 0)
            cpu += info["cpu"]
            threads += info["threads"]
        return {
            "processes": len(seen), "rss": rss, "private": private, "pss": pss,
            "cpu": cpu, "threads": threads,
        }


def percentile(samples, percent):
    if not samples:
        return 0
    ordered = sorted(samples)
    index = min(len(ordered) - 1, max(0, int(round(percent / 100.0 * len(ordered))) - 1))
    return ordered[index]


def summarize(samples):
    if not samples:
        return {"n": 0, "min": 0, "avg": 0, "p50": 0, "p95": 0, "p99": 0, "max": 0}
    return {
        "n": len(samples),
        "min": min(samples),
        "avg": sum(samples) / len(samples),
        "p50": percentile(samples, 50),
        "p95": percentile(samples, 95),
        "p99": percentile(samples, 99),
        "max": max(samples),
    }


def sample_process(pid):
    """RSS/CPU/threads for one process (the daemon itself)."""
    if not pid:
        return None
    if WIN:
        info = _process_info(pid)
        if info is None:
            return None
        threads = _process_table().get(pid, {}).get("threads", 0)
        return dict(info, threads=threads)
    status = _read_status(pid)
    stat = _read_stat(pid)
    if status is None or stat is None:
        return None
    return {"rss": status["rss"], "private": status["rss"], "cpu": stat["cpu"], "threads": status["threads"]}


# --------------------------------------------------------------------------
# fixture and daemon lifecycle
# --------------------------------------------------------------------------

AGENT = """#!/usr/bin/env python3
import json, sys, threading
lock = threading.Lock()
event = 0
def send(message):
    global event
    line = json.dumps(message, separators=(",", ":"))
    with lock:
        sys.stdout.write(line + "\\n")
        sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        request = json.loads(line)
    except Exception:
        continue
    request_id = request.get("id")
    method = request.get("method")
    if method == "svcd.ping":
        send({"jsonrpc": "2.0", "id": request_id, "result": {"ok": True}})
    elif method == "tools/call":
        event += 1
        send({"jsonrpc": "2.0", "method": "notifications/message",
              "params": {"level": "info", "data": {"event": event}}})
        send({"jsonrpc": "2.0", "id": request_id,
              "result": {"content": [{"type": "text", "text": "ok"}], "isError": False}})
    elif request_id is not None:
        send({"jsonrpc": "2.0", "id": request_id,
              "error": {"code": -32601, "message": "method not found"}})
"""


SHELL_AGENT = """#!/bin/bash
# NDJSON agent with no interpreter: the Linux fixture should not pay for a
# Python process per plugin. Bash parameter expansion keeps the id parse cheap.
while IFS= read -r line; do
  rest=${line#*id*:}
  id=${rest%%[!0-9]*}
  if [ -z "$id" ]; then id=1; fi
  case "$line" in
    *svcd.ping*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      ;;
    *tools/call*)
      printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":{"tick":1}}}\n'
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}\n' "$id"
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      ;;
  esac
done
"""


def write_fixture(root, runtime="python"):
    fixture = root / "plugin"
    (fixture / "payload" / "bin").mkdir(parents=True, exist_ok=True)
    (fixture / "payload" / "web").mkdir(parents=True, exist_ok=True)
    manifest = {
        "schema": 1,
        "name": "budget",
        "version": "1.0.0",
        "desc": "budget scenario fixture",
        "protocol": 1,
        "exec": {"path": "payload/bin/agent.sh", "ready": "notify"},
        "capabilities": [{"name": "echo", "desc": "echo", "inputSchema": {"type": "object"}}],
        "ui": {"type": "web", "entry": "index.html"},
    }
    if runtime == "shell":
        if WIN:
            raise SystemExit("the shell plugin runtime is Unix-only")
        script = fixture / "payload" / "bin" / "agent.sh"
        script.write_text(SHELL_AGENT, encoding="utf-8")
        script.chmod(0o755)
    else:
        (fixture / "payload" / "bin" / "agent.py").write_text(AGENT, encoding="utf-8")
        if WIN:
            manifest["exec"]["path"] = "payload/bin/agent.cmd"
        else:
            script = fixture / "payload" / "bin" / "agent.sh"
            script.write_text('#!/bin/sh\nexec python3 "$(dirname "$0")/agent.py"\n', encoding="utf-8")
            script.chmod(0o755)
    (fixture / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    (fixture / "payload" / "web" / "index.html").write_text(
        "<!doctype html><html><body>budget</body></html>\n", encoding="utf-8"
    )
    if runtime != "shell" and WIN:
        (fixture / "payload" / "bin" / "agent.cmd").write_text(
            '@echo off\r\npython "%~dp0agent.py"\r\n', encoding="utf-8", newline=""
        )
    return fixture


def start_daemon(root, binary, budget_bytes, trust_pub, cpu_percent=0):
    config = root / "config"
    (config / "trust.d").mkdir(parents=True, exist_ok=True)
    (root / "data").mkdir(parents=True, exist_ok=True)
    (root / "logs").mkdir(parents=True, exist_ok=True)
    (root / "bin").mkdir(parents=True, exist_ok=True)
    shutil.copyfile(trust_pub, config / "trust.d" / "sample-2026.pub")
    # MCP authenticates Bearer against auth_passwd (src/server/auth/lan.rs), not
    # against the legacy lan-token file. The management API stays unauthenticated
    # on loopback because LAN is disabled.
    password = "budget-test-password"
    salt = os.urandom(16)
    digest = hashlib.pbkdf2_hmac("sha256", password.encode("utf-8"), salt, 10000)
    encode = lambda value: base64.urlsafe_b64encode(value).decode("ascii").rstrip("=")
    password_hash = "pbkdf2-sha256$10000$%s$%s" % (encode(salt), encode(digest))
    document = ["bind = 127.0.0.1:0", "mem_budget_bytes = %d" % budget_bytes]
    if cpu_percent:
        document.append("cpu_budget_percent = %d" % cpu_percent)
    document.append("auth_passwd = %s" % password_hash)
    (config / "svcd.conf").write_text("\n".join(document) + "\n", encoding="utf-8")
    env = os.environ.copy()
    env.update({
        "SVCD_CONFIG_DIR": str(config),
        "SVCD_DATA_DIR": str(root / "data"),
        "SVCD_LOG_DIR": str(root / "logs"),
        "SVCD_TRUST_DIR": str(config / "trust.d"),
        "SVCD_BIND": "127.0.0.1:0",
        "SVCD_TEST_UNPRIVILEGED": "1",
    })
    log_path = root / "daemon.stderr"
    log_file = open(log_path, "a", encoding="utf-8")
    process = subprocess.Popen(
        [str(binary), "run", "--foreground"],
        env=env, cwd=str(root), stdout=log_file, stderr=subprocess.STDOUT,
    )
    try:
        address = wait_for_http(root, process)
    except Exception:
        process.kill()
        log_file.close()
        raise
    return {
        "process": process, "env": env, "address": address,
        "token": password, "log": log_file, "log_path": log_path,
    }


def stop_daemon(root, daemon):
    svc = root / "bin" / ("svc" + EXE)
    try:
        subprocess.run([str(svc), "shutdown"], env=daemon["env"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=10, cwd=str(root))
    except Exception:
        pass
    try:
        daemon["process"].wait(timeout=5)
    except Exception:
        daemon["process"].terminate()
        try:
            daemon["process"].wait(timeout=5)
        except Exception:
            daemon["process"].kill()
    try:
        daemon["log"].close()
    except Exception:
        pass


def bundle_plugins(root, binary, env, fixture, plugins, key):
    alias = root / "bin" / ("plugin" + EXE)
    shutil.copyfile(binary, alias)
    if not WIN:
        alias.chmod(0o755)
    packages = {}
    for plugin_id in plugins:
        spk = root / (plugin_id + ".spk")
        result = subprocess.run(
            [str(alias), "bundle", str(fixture), "--id", plugin_id,
             "--key", str(key), "--key-id", "sample-2026", "-o", str(spk)],
            env=env, cwd=str(root), capture_output=True, text=True, timeout=60,
        )
        if result.returncode != 0 or not spk.is_file():
            raise RuntimeError("bundle %s failed: %s%s" % (plugin_id, result.stdout, result.stderr))
        packages[plugin_id] = spk.read_bytes()
    return packages


def detect_platform():
    """The target triple the binary was built for (build.rs bakes cargo TARGET)."""
    try:
        result = subprocess.run(
            ["rustc", "-vV"], capture_output=True, text=True, timeout=15
        )
        match = re.search(r"^host: (.+)$", result.stdout, re.MULTILINE)
        if match:
            return match.group(1).strip()
    except Exception:
        pass
    if WIN:
        return "x86_64-pc-windows-msvc"
    machine = hostinfo.machine()
    return {
        "x86_64": "x86_64-unknown-linux-gnu",
        "aarch64": "aarch64-unknown-linux-gnu",
    }.get(machine, machine + "-unknown-linux-gnu")


def write_direct_layout(root, fixture, plugins, target):
    """Install the fixture without the control channel.

    Some sandboxes deny the daemon's named pipe to every other process, which
    makes the management API fail with "svcd unreachable". The registry only
    reads <data>/plugins/<id>/{current,enabled,<version>/manifest.json}, so the
    same runtime layout can be written directly. This skips signature
    verification and staging; it exists for measurement hosts whose control
    channel is unavailable.
    """
    version = "1.0.0"
    plugins_dir = root / "data" / "plugins"
    for plugin_id in plugins:
        directory = plugins_dir / plugin_id
        destination = directory / version
        shutil.copytree(fixture, destination)
        manifest_path = destination / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["platform"] = target
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        (directory / "current").write_text(version, encoding="utf-8")
        (directory / "enabled").write_text("", encoding="utf-8")
    return plugins_dir


def spawned_pids(daemon):
    """Pids the daemon reported at boot (works when the management API does not)."""
    try:
        text = Path(daemon["log_path"]).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return {}
    pattern = re.compile(r"svcd: ([A-Za-z0-9_.-]+): spawned pid (\d+)")
    return {match.group(1): int(match.group(2)) for match in pattern.finditer(text)}


def live_plugins(daemon, plugins):
    pids = spawned_pids(daemon)
    live = {}
    for plugin_id in plugins:
        pid = pids.get(plugin_id)
        if pid and sample_process(pid):
            live[plugin_id] = pid
    return live


def wait_for_live(daemon, plugins, timeout=30.0):
    deadline = time.time() + timeout
    live = {}
    while time.time() < deadline:
        live = live_plugins(daemon, plugins)
        if len(live) == len(plugins):
            return live
        time.sleep(0.2)
    return live


def install_and_start(address, packages, plugins):
    started = {}
    for plugin_id in plugins:
        status, _, body = http_request(
            address, "POST", "/api/v1/plugins",
            {"content-type": "application/octet-stream"}, packages[plugin_id], timeout=30,
        )
        if status != 200:
            raise RuntimeError("install %s: HTTP %s %s" % (plugin_id, status, body[:200]))
        status, _, body = http_request(
            address, "POST", "/api/v1/services/%s/start" % plugin_id,
            {"content-length": "0"}, None, timeout=30,
        )
        if status != 200:
            raise RuntimeError("start %s: HTTP %s %s" % (plugin_id, status, body[:200]))
        status, _, body = http_request(
            address, "GET", "/api/v1/services/%s" % plugin_id, {}, None, timeout=10,
        )
        payload = read_json(body) or {}
        summary = payload.get("summary") or {}
        started[plugin_id] = {
            "pid": summary.get("pid"),
            "state": summary.get("state") or summary.get("status"),
            "raw": payload,
        }
    return started


# --------------------------------------------------------------------------
# traffic
# --------------------------------------------------------------------------

def op_invoke(address, token, plugin_id, sequence):
    request = {
        "jsonrpc": "2.0",
        "id": 100000 + sequence,
        "method": "tools/call",
        "params": {
            "name": plugin_id + ".echo",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        },
    }
    status, _, body = http_request(
        address, "POST", "/mcp",
        {
            "Authorization": "Bearer " + token,
            "Content-Type": "application/json",
            "Accept": "application/json",
            "MCP-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": plugin_id + ".echo",
        },
        json.dumps(request).encode("utf-8"), timeout=10,
    )
    payload = read_json(body) or {}
    if status != 200:
        # Name the refusal: "503 http-503" cannot tell a saturated blocking
        # pool from the connection cap, and the answer differs per tier.
        code = (payload.get("error") or {}).get("code") or "http-%s" % status
        return False, status, code
    if "error" in payload:
        return False, status, "rpc-%s" % (payload["error"] or {}).get("code")
    ok = "result" in payload and not (payload.get("result") or {}).get("isError", False)
    return ok, status, "mcp" if ok else "no-result"


def plugin_token(address, plugin_id, tokens):
    """Mint the page token the stream routes require (in-process, no control call)."""
    cached = tokens.get(plugin_id)
    if cached:
        return cached
    status, _, body = http_request(
        address, "POST", "/api/v1/plugins/%s/ui-token" % plugin_id,
        {"content-length": "0"}, None, timeout=10,
    )
    if status != 200:
        raise RuntimeError("ui-token %s: HTTP %s %s" % (plugin_id, status, body[:120]))
    token = (read_json(body) or {}).get("token")
    if not token:
        raise RuntimeError("ui-token %s: empty token" % plugin_id)
    tokens[plugin_id] = token
    return token


def op_sse(address, token, plugin_id, sequence, tokens):
    host, port = address.rsplit(":", 1)
    ui_token = plugin_token(address, plugin_id, tokens)
    connection = http.client.HTTPConnection(host, int(port), timeout=10)
    try:
        connection.request(
            "GET", "/plugin-ui/%s/%s/events" % (plugin_id, ui_token),
            headers={"Accept": "text/event-stream", "Connection": "close"},
        )
        response = connection.getresponse()
        if response.status != 200:
            return False, response.status, "sse"
        if connection.sock is not None:
            connection.sock.settimeout(5.0)
        ok, status, detail = op_invoke(address, token, plugin_id, sequence)
        if not ok:
            return False, status, "sse-trigger:%s" % detail
        data_lines = []
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            line = response.readline()
            if not line:
                break
            text = line.decode("utf-8", "replace").rstrip("\r\n")
            if text == "":
                if data_lines:
                    return True, 200, "sse"
                continue
            if text.startswith("data:"):
                data_lines.append(text[5:].lstrip())
        return False, 200, "sse-no-event"
    finally:
        connection.close()


def _recv_exact(sock, count):
    data = b""
    while len(data) < count:
        chunk = sock.recv(count - len(data))
        if not chunk:
            raise ConnectionError("socket closed")
        data += chunk
    return data


def _recv_until(sock, marker):
    data = b""
    while marker not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    return data


def _ws_send_frame(sock, opcode, text):
    payload = text.encode("utf-8")
    header = bytearray([0x80 | opcode])
    if len(payload) < 126:
        header.append(0x80 | len(payload))
    elif len(payload) < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", len(payload))
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", len(payload))
    mask = os.urandom(4)
    header += mask
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    sock.sendall(bytes(header) + masked)


def _ws_recv(sock):
    first, second = _recv_exact(sock, 2)
    opcode = first & 0x0F
    masked = second & 0x80
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", _recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", _recv_exact(sock, 8))[0]
    mask = _recv_exact(sock, 4) if masked else None
    payload = _recv_exact(sock, length) if length else b""
    if mask:
        payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return opcode, payload


def _ws_open(address, plugin_id, ui_token, timeout=5.0):
    host, port = address.rsplit(":", 1)
    sock = socket.create_connection((host, int(port)), timeout=timeout)
    key = base64.b64encode(os.urandom(16)).decode("ascii")
    request = (
        "GET /plugin-ui/%s/%s/ws HTTP/1.1\r\n"
        "Host: %s\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "Sec-WebSocket-Key: %s\r\n"
        "\r\n"
    ) % (plugin_id, ui_token, address, key)
    sock.sendall(request.encode("ascii"))
    head = _recv_until(sock, b"\r\n\r\n")
    if not head.startswith(b"HTTP/1.1"):
        sock.close()
        raise ConnectionError("bad websocket response")
    status = int(head.split(b" ")[1])
    if status != 101:
        sock.close()
        return None, status
    return sock, status


def op_ws(address, plugin_id, sequence, tokens):
    ui_token = plugin_token(address, plugin_id, tokens)
    sock, status = _ws_open(address, plugin_id, ui_token)
    if sock is None:
        return False, status, "ws-handshake"
    try:
        request_id = 200000 + sequence
        _ws_send_frame(sock, 0x1, json.dumps({
            "jsonrpc": "2.0", "id": request_id, "method": "tools/call",
            "params": {"name": plugin_id + ".echo", "arguments": {}},
        }))
        deadline = time.monotonic() + 5
        sock.settimeout(5.0)
        while time.monotonic() < deadline:
            opcode, payload = _ws_recv(sock)
            if opcode == 0x9:
                _ws_send_frame(sock, 0xA, payload.decode("utf-8", "replace"))
                continue
            if opcode == 0x8:
                return False, 200, "ws-close"
            if opcode != 0x1:
                continue
            message = read_json(payload) or {}
            if message.get("id") == request_id:
                if "error" in message:
                    return False, 200, "rpc-%s" % (message["error"] or {}).get("code")
                ok = "result" in message and not (message.get("result") or {}).get("isError", False)
                return ok, 200, "ws" if ok else "no-result"
        return False, 200, "ws-timeout"
    finally:
        try:
            sock.close()
        except Exception:
            pass


def op_upload(address, plugin_id, sequence, blobs):
    # One constant body per plugin: the blob store dedupes it, so a 60 s run at
    # 1000 workers cannot fill the disk, while the write path is still exercised.
    seed = ("%s:" % plugin_id).encode("ascii")
    body = seed + b"x" * (4096 - len(seed))
    status, headers, _ = http_request(
        address, "POST", "/api/v1/blobs",
        {"content-type": "application/octet-stream"}, body, timeout=15,
    )
    location = headers.get("location")
    if status != 200 or not location:
        return False, status, "upload"
    blobs[plugin_id] = {"location": location, "length": len(body)}
    return True, status, "upload"


def op_download(address, plugin_id, sequence, blobs):
    if plugin_id not in blobs:
        ok, status, detail = op_upload(address, plugin_id, sequence, blobs)
        if not ok:
            return False, status, "download-prefetch"
    record = blobs[plugin_id]
    status, _, body = http_request(address, "GET", record["location"], {}, None, timeout=15)
    ok = status == 200 and len(body) == record["length"]
    return ok, status, "download"


def execute(address, token, plugin_id, op, sequence, blobs, tokens):
    if op == "invoke":
        return op_invoke(address, token, plugin_id, sequence)
    if op == "sse":
        return op_sse(address, token, plugin_id, sequence, tokens)
    if op == "ws":
        return op_ws(address, plugin_id, sequence, tokens)
    if op == "upload":
        return op_upload(address, plugin_id, sequence, blobs)
    return op_download(address, plugin_id, sequence, blobs)


def run_traffic(address, token, plugins, duration, global_concurrency, per_plugin, think_ms):
    """Drive every plugin through all five surfaces.

    per_plugin > 0 pins that many workers to each plugin (10 x 50 = 500, 10 x
    100 = 1000); otherwise global_concurrency workers share one round-robin
    schedule. Peak in-flight counts are tracked per surface, which is the
    connection concurrency the daemon actually admitted.
    """
    guard = threading.Lock()
    deadline = time.monotonic() + duration
    blobs = {}
    tokens = {}
    for plugin in plugins:
        plugin_token(address, plugin, tokens)
    results = {
        plugin: {
            op: {
                "attempts": 0, "ok": 0, "errors": 0,
                "statuses": {}, "details": {}, "latencies": [],
            }
            for op in OPS
        }
        for plugin in plugins
    }
    active = {op: 0 for op in OPS}
    peak = {op: 0 for op in OPS}
    peak_total = [0]
    global_ = {
        op: {"attempts": 0, "ok": 0, "errors": 0, "statuses": {}, "latencies": []}
        for op in OPS
    }

    def enter(op):
        with guard:
            active[op] += 1
            if active[op] > peak[op]:
                peak[op] = active[op]
            total = sum(active.values())
            if total > peak_total[0]:
                peak_total[0] = total

    def leave(op):
        with guard:
            active[op] -= 1

    def record(plugin, op, ok, status, elapsed, detail):
        bucket = results[plugin][op]
        with guard:
            bucket["attempts"] += 1
            bucket["statuses"][str(status)] = bucket["statuses"].get(str(status), 0) + 1
            bucket["details"][str(detail)] = bucket["details"].get(str(detail), 0) + 1
            overall = global_[op]
            overall["attempts"] += 1
            overall["statuses"][str(status)] = overall["statuses"].get(str(status), 0) + 1
            if ok:
                bucket["ok"] += 1
                bucket["latencies"].append(elapsed * 1000)
                overall["ok"] += 1
                overall["latencies"].append(elapsed * 1000)
            else:
                bucket["errors"] += 1
                overall["errors"] += 1

    def run_one(plugin, op, sequence):
        enter(op)
        began = time.monotonic()
        try:
            ok, status, detail = execute(address, token, plugin, op, sequence, blobs, tokens)
        except Exception as error:  # noqa: BLE001 - reported, not swallowed
            ok, status, detail = False, type(error).__name__, str(error)
        elapsed = time.monotonic() - began
        leave(op)
        record(plugin, op, ok, status, elapsed, detail)
        if think_ms > 0:
            time.sleep(think_ms / 1000.0)

    def plugin_worker(plugin):
        sequence = 0
        while time.monotonic() < deadline:
            op = OPS[sequence % len(OPS)]
            sequence += 1
            run_one(plugin, op, sequence)

    schedule = [(plugin, op) for plugin in plugins for op in OPS]
    cursor = [0]

    def global_worker():
        while time.monotonic() < deadline:
            with guard:
                index = cursor[0]
                cursor[0] += 1
            plugin, op = schedule[index % len(schedule)]
            run_one(plugin, op, index)

    if per_plugin > 0:
        workers = per_plugin * len(plugins)
        threads = []
        for plugin in plugins:
            for _ in range(per_plugin):
                threads.append(threading.Thread(target=plugin_worker, args=(plugin,), daemon=True))
    else:
        workers = global_concurrency
        threads = [threading.Thread(target=global_worker, daemon=True) for _ in range(global_concurrency)]
    if workers > 200:
        try:
            threading.stack_size(256 * 1024)
        except (ValueError, RuntimeError):
            pass
    began = time.monotonic()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    elapsed = time.monotonic() - began
    merged = []
    for plugin in plugins:
        for op in OPS:
            bucket = results[plugin][op]
            merged.extend(bucket["latencies"])
            bucket["latency"] = summarize(bucket.pop("latencies"))
    for op in OPS:
        overall = global_[op]
        overall["latency"] = summarize(overall.pop("latencies"))
        overall["rps"] = overall["ok"] / elapsed if elapsed else 0
    total_ok = sum(global_[op]["ok"] for op in OPS)
    total_attempts = sum(global_[op]["attempts"] for op in OPS)
    return results, {
        "elapsed_seconds": elapsed,
        "workers": workers,
        "peak_active": peak,
        "peak_total": peak_total[0],
        "summary": global_,
        # Every surface in one distribution: a single latency answer for the run,
        # next to the per-surface ones a slow individual surface would hide in.
        "overall_latency": summarize(merged),
        "total_ok": total_ok,
        "total_attempts": total_attempts,
        "total_rps": total_ok / elapsed if elapsed else 0,
    }


def oversubscribe(address, plugins, hold=1.5):
    host, port = address.rsplit(":", 1)
    outcomes = {"sse": [], "ws": []}
    release = threading.Event()
    lock = threading.Lock()
    tokens = {}
    for plugin_id in plugins:
        plugin_token(address, plugin_id, tokens)

    def append(kind, value):
        with lock:
            outcomes[kind].append(value)

    def sse_probe(plugin_id):
        try:
            connection = http.client.HTTPConnection(host, int(port), timeout=5)
            connection.request(
                "GET", "/plugin-ui/%s/%s/events" % (plugin_id, tokens[plugin_id]),
                headers={"Accept": "text/event-stream", "Connection": "close"},
            )
            response = connection.getresponse()
            append("sse", response.status)
            if response.status == 200:
                release.wait(hold + 3)
            connection.close()
        except Exception as error:  # noqa: BLE001
            append("sse", type(error).__name__)

    def ws_probe(plugin_id):
        try:
            sock, status = _ws_open(address, plugin_id, tokens[plugin_id])
            append("ws", status)
            if sock is not None:
                release.wait(hold + 3)
                sock.close()
        except Exception as error:  # noqa: BLE001
            append("ws", type(error).__name__)

    threads = []
    for plugin_id in plugins:
        threads.append(threading.Thread(target=sse_probe, args=(plugin_id,), daemon=True))
        threads.append(threading.Thread(target=ws_probe, args=(plugin_id,), daemon=True))
    for thread in threads:
        thread.start()
    time.sleep(hold)
    release.set()
    for thread in threads:
        thread.join(timeout=10)
    return outcomes


def parse_tune(text):
    limits = {}
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        limits[key.strip()] = value.strip()
    return limits


def render_report(result):
    effective = result.get("effective") or {}
    traffic = result.get("traffic") or {}
    resource = result.get("resources") or {}
    requested = result.get("requested") or {}
    meta = result.get("traffic_meta") or {}
    summary = meta.get("summary") or {}
    lines = []
    lines.append("# Memory-budget scenario: %d plugins, B=%s" % (
        requested.get("plugins"), requested.get("budget_text")))
    lines.append("")
    lines.append("Generated: " + result["generated_at"])
    lines.append("")
    lines.append("- OS: %s" % result["environment"]["os"])
    lines.append("- CPU: %s x%d" % (result["environment"]["cpu"], result["environment"]["cpus"]))
    lines.append("- Requested budget: %d bytes (%s)" % (
        requested.get("budget_bytes"), requested.get("budget_text")))
    lines.append("- Effective budget: %s bytes, workload %s, worst case %s bytes" % (
        effective.get("budget_quantized"), effective.get("workload"), effective.get("worst_case")))
    lines.append("- CPU budget: %s%%" % (requested.get("cpu_budget_percent") or "off"))
    lines.append("- Plugin runtime: %s" % requested.get("plugin_runtime", "auto"))
    lines.append("- Duration: %.2fs, workers %s (per plugin %s), think %sms" % (
        meta.get("elapsed_seconds", 0), meta.get("workers", 0),
        requested.get("per_plugin_concurrency", 0), requested.get("think_ms", 0)))
    lines.append("- Ops: %s ok / %s attempts, %.1f ops/s" % (
        meta.get("total_ok", 0), meta.get("total_attempts", 0), meta.get("total_rps", 0)))
    lines.append("")
    lines.append("## Derived limits")
    lines.append("")
    lines.append("| key | value |")
    lines.append("| --- | ---: |")
    for key in ("max_connections", "max_connections_per_ip", "max_streams",
                "max_subscriptions", "max_subscribers_per_plugin",
                "max_complete_body_bytes", "max_body_bytes"):
        lines.append("| %s | %s |" % (key, result["limits"].get(key, "")))
    lines.append("")
    lines.append("## Traffic per plugin (ok/attempts)")
    lines.append("")
    lines.append("| plugin | " + " | ".join(OPS) + " |")
    lines.append("| --- | " + " | ".join("---:" for _ in OPS) + " |")
    for plugin in traffic:
        cells = []
        for op in OPS:
            bucket = traffic[plugin][op]
            cells.append("%d/%d" % (bucket["ok"], bucket["attempts"]))
        lines.append("| %s | %s |" % (plugin, " | ".join(cells)))
    lines.append("")
    lines.append("## Throughput and latency")
    lines.append("")
    lines.append("| surface | attempts | ok | errors | rps | p50 ms | p95 ms | p99 ms | max ms | peak concurrent |")
    lines.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    peak = meta.get("peak_active") or {}
    overall = meta.get("overall_latency") or {}
    if overall:
        lines.append("| all surfaces | %s | %s | %s | %.1f | %.1f | %.1f | %.1f | %.1f | %s |" % (
            meta.get("total_attempts", 0), meta.get("total_ok", 0),
            meta.get("total_attempts", 0) - meta.get("total_ok", 0),
            meta.get("total_rps", 0), overall.get("p50", 0), overall.get("p95", 0),
            overall.get("p99", 0), overall.get("max", 0), meta.get("peak_total", 0)))
    for op in OPS:
        item = summary.get(op) or {}
        latency = item.get("latency") or {}
        lines.append("| %s | %s | %s | %s | %.1f | %.1f | %.1f | %.1f | %.1f | %s |" % (
            op, item.get("attempts", 0), item.get("ok", 0), item.get("errors", 0),
            item.get("rps", 0), latency.get("p50", 0), latency.get("p95", 0),
            latency.get("p99", 0), latency.get("max", 0), peak.get(op, 0)))
    lines.append("")
    lines.append("## Connections")
    lines.append("")
    lines.append("| item | value |")
    lines.append("| --- | ---: |")
    lines.append("| peak in-flight total | %s |" % meta.get("peak_total", 0))
    for op in OPS:
        lines.append("| peak %s | %s |" % (op, peak.get(op, 0)))
    lines.append("| max_connections | %s |" % result["limits"].get("max_connections", ""))
    lines.append("| max_connections_per_ip | %s |" % result["limits"].get("max_connections_per_ip", ""))
    lines.append("| max_streams | %s |" % result["limits"].get("max_streams", ""))
    lines.append("| max_subscriptions | %s |" % result["limits"].get("max_subscriptions", ""))
    lines.append("")
    lines.append("## Resources")
    lines.append("")
    lines.append("| sample | avg | max |")
    lines.append("| --- | ---: | ---: |")
    if (resource.get("tree_pss") or {}).get("max", 0) > 0:
        item = resource["tree_pss"]
        lines.append("| process tree PSS MB | %.2f | %.2f |" % (item["avg"] / MIB, item["max"] / MIB))
    for key, label, unit in (
        ("daemon_rss", "daemon RSS MB", MIB),
        ("spk_rss", "spk RSS MB", MIB),
        ("tree_rss", "process tree RSS MB", MIB),
        ("daemon_threads", "daemon threads", 1),
        ("tree_threads", "process tree threads", 1),
        ("processes", "processes", 1),
    ):
        item = resource.get(key) or {"avg": 0, "max": 0}
        lines.append("| %s | %.2f | %.2f |" % (
            label, item["avg"] / unit, item["max"] / unit))
    lines.append("")
    lines.append("- daemon CPU (one core): %.2f%%" % resource.get("daemon_cpu_percent", 0))
    lines.append("- process tree CPU (one core): %.2f%%" % resource.get("tree_cpu_percent", 0))
    lines.append("- RSS samples: %s (every 2s)" % resource.get("samples", 0))
    if result.get("oversubscription", {}).get("sse"):
        lines.append("")
        lines.append("## Oversubscription")
        lines.append("")
        lines.append("- SSE probes: %s" % result["oversubscription"]["sse"])
        lines.append("- WS probes: %s" % result["oversubscription"]["ws"])
        lines.append("- recovery invoke: %s" % result["oversubscription"]["recovery"])
    failures = []
    for plugin in traffic:
        for op in OPS:
            bucket = traffic[plugin][op]
            if bucket["errors"]:
                failures.append("- %s %s: %d/%d errors; %s" % (
                    plugin, op, bucket["errors"], bucket["attempts"],
                    json.dumps(bucket["details"], sort_keys=True)))
    if failures:
        lines.append("")
        lines.append("## Failures")
        lines.append("")
        lines.extend(failures)
    lines.append("")
    lines.append("## Health")
    lines.append("")
    health = result.get("health") or {}
    lines.append("- daemon alive at end: %s" % health.get("daemon_alive"))
    lines.append("- services running: %s/%s" % (
        health.get("running"), health.get("expected")))
    lines.append("- coverage: %s (strict every plugin x op: %s)" % (
        health.get("coverage"), health.get("coverage_strict")))
    lines.append("- statuses: %s" % json.dumps(health.get("statuses") or {}, sort_keys=True))
    lines.append("")
    return "\n".join(lines)


def service_identity(entry):
    summary = entry.get("summary") or {}
    identity = entry.get("id") or entry.get("name") or summary.get("id")
    state = summary.get("state") or entry.get("state") or summary.get("status")
    return identity, state


def count_statuses(traffic):
    counts = {}
    for plugin in traffic:
        for op in OPS:
            for status, count in traffic[plugin][op]["statuses"].items():
                counts[status] = counts.get(status, 0) + count
    return counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=os.path.join("target", "release", "svcd" + EXE))
    parser.add_argument("--budget", type=parse_size, default=parse_size("50mb"),
                        help="mem_budget_bytes to request (default 50mb)")
    parser.add_argument("--budget-text", default=None, help="label for the requested budget")
    parser.add_argument("--plugins", type=int, default=10)
    parser.add_argument("--duration", type=float, default=60.0)
    parser.add_argument("--concurrency", type=int, default=4,
                        help="shared workers when --per-plugin-concurrency is 0")
    parser.add_argument("--per-plugin-concurrency", type=int, default=0,
                        help="workers pinned to each plugin (10 plugins x 50 = 500)")
    parser.add_argument("--think-ms", type=float, default=20.0,
                        help="pause after each operation, to keep a refused run from spinning")
    parser.add_argument("--plugin-runtime", choices=("auto", "python", "shell"), default="auto",
                        help="shell uses a bash NDJSON agent on Linux; python is the Windows fixture")
    parser.add_argument("--traffic", choices=("all", "idle"), default="all",
                        help="all drives every surface; idle only boots the plugins and samples")
    parser.add_argument("--cpu-budget-percent", type=int, default=0,
                        help="cpu_budget_percent in the document (0 disables the governor)")
    parser.add_argument("--no-oversubscribe", action="store_true")
    parser.add_argument("--bootstrap", choices=("api", "direct"), default="api",
                        help="api installs through the management API; direct writes the "
                             "registry layout before boot for hosts whose control channel "
                             "is unavailable")
    parser.add_argument("--key", default=os.path.join("samples", "keys", "sample-2026.key"))
    parser.add_argument("--pub", default=os.path.join("samples", "keys", "sample-2026.pub"))
    parser.add_argument("--work", default=None)
    parser.add_argument("--out", default=os.path.join("target", "budget-results.json"))
    parser.add_argument("--report", default=os.path.join("target", "budget-report.md"))
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[2]
    binary = Path(args.binary)
    if not binary.is_absolute():
        binary = repo / binary
    key = Path(args.key)
    if not key.is_absolute():
        key = repo / key
    pub = Path(args.pub)
    if not pub.is_absolute():
        pub = repo / pub
    if not binary.is_file():
        raise SystemExit("missing binary: %s (run cargo build --release)" % binary)
    if not key.is_file() or not pub.is_file():
        generator = repo / "tools" / "gen_sample_key.mjs"
        node = shutil.which("node")
        if generator.is_file() and node:
            subprocess.run([node, str(generator)], cwd=str(repo), check=True)
    if not key.is_file() or not pub.is_file():
        raise SystemExit("missing sample keys under %s" % (repo / "samples" / "keys"))

    work_root = Path(args.work) if args.work else repo / "target"
    work_root.mkdir(parents=True, exist_ok=True)
    root = work_root / ("budget-run-" + time.strftime("%Y%m%d-%H%M%S"))
    root.mkdir(parents=True, exist_ok=False)
    plugins = ["p%02d" % index for index in range(args.plugins)]
    budget_text = args.budget_text or ("%gmb" % (args.budget / MIB))
    log("workdir %s" % root)
    log("requested budget %d bytes (%s), %d plugins, %ss, workers %d" % (
        args.budget, budget_text, args.plugins, args.duration,
        args.per_plugin_concurrency * args.plugins
        if args.per_plugin_concurrency else args.concurrency))

    daemon = None
    samples = []
    stop = threading.Event()
    result = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "workdir": str(root),
        "environment": {
            "os": ("Windows" if WIN else "Linux") + " " + os.uname().machine if not WIN else "Windows " + os.environ.get("PROCESSOR_ARCHITECTURE", "x86_64"),
            "cpu": (os.environ.get("PROCESSOR_IDENTIFIER", "") if WIN else os.uname().machine),
            "cpus": os.cpu_count() or 1,
        },
        "requested": {
            "budget_bytes": args.budget,
            "budget_text": budget_text,
            "plugins": args.plugins,
            "duration": args.duration,
            "concurrency": args.concurrency,
            "per_plugin_concurrency": args.per_plugin_concurrency,
            "workers": (
                args.per_plugin_concurrency * args.plugins
                if args.per_plugin_concurrency else args.concurrency
            ),
            "think_ms": args.think_ms,
            "cpu_budget_percent": args.cpu_budget_percent,
            "plugin_runtime": args.plugin_runtime,
            "traffic": args.traffic,
        },
        "effective": None,
        "limits": {},
        "traffic": {},
        "resources": {},
        "oversubscription": {"sse": [], "ws": [], "recovery": None},
        "health": {},
    }
    exit_code = 1
    try:
        runtime = args.plugin_runtime
        if runtime == "auto":
            runtime = "shell" if not WIN else "python"
        fixture = write_fixture(root, runtime)
        result["requested"]["plugin_runtime"] = runtime
        log("plugin runtime %s" % runtime)
        if args.bootstrap == "direct":
            target = detect_platform()
            write_direct_layout(root, fixture, plugins, target)
            log("direct layout for %d plugins (platform %s)" % (len(plugins), target))
        daemon = start_daemon(root, binary, args.budget, pub, args.cpu_budget_percent)
        log("daemon on %s, pid %d" % (daemon["address"], daemon["process"].pid))
        result["effective"] = startup_report(root)
        svc = root / "bin" / ("svc" + EXE)
        shutil.copyfile(binary, svc)
        if not WIN:
            svc.chmod(0o755)
        tune = subprocess.run([str(svc), "config", "tune"], env=daemon["env"],
                              cwd=str(root), capture_output=True, text=True, timeout=30)
        result["limits"] = parse_tune(tune.stdout)
        log("effective budget %s, workload %s, limits: connections=%s streams=%s subscriptions=%s" % (
            (result["effective"] or {}).get("budget_quantized"),
            (result["effective"] or {}).get("workload"),
            result["limits"].get("max_connections"),
            result["limits"].get("max_streams"),
            result["limits"].get("max_subscriptions"),
        ))

        if args.bootstrap == "direct":
            live = wait_for_live(daemon, plugins)
            started = {
                plugin_id: {"pid": pid, "state": "running", "raw": {}}
                for plugin_id, pid in live.items()
            }
        else:
            packages = bundle_plugins(root, binary, daemon["env"], fixture, plugins, key)
            started = install_and_start(daemon["address"], packages, plugins)
        running_pids = [info["pid"] for info in started.values() if info["pid"]]
        log("started %d plugins (%d pids)" % (len(started), len(running_pids)))

        def sampler():
            while not stop.wait(2.0):
                # A plugin child can exit between the /proc listing and the read;
                # one vanished pid must not end the sampler (the run then reports
                # CPU 0 and one RSS sample, which reads like a healthy daemon).
                try:
                    daemon_sample = sample_process(daemon["process"].pid)
                    tree_sample = process_tree(daemon["process"].pid)
                except Exception as error:  # noqa: BLE001 - sampled, not swallowed
                    log("sample skipped: %r" % (error,))
                    continue
                samples.append({
                    "at": time.time(),
                    "daemon": daemon_sample,
                    "tree": tree_sample,
                })

        sampler_thread = threading.Thread(target=sampler, daemon=True)
        sampler_thread.start()
        began = time.monotonic()
        if args.traffic == "all":
            traffic, traffic_meta = run_traffic(
                daemon["address"], daemon["token"], plugins, args.duration,
                args.concurrency, args.per_plugin_concurrency, args.think_ms,
            )
        else:
            while time.monotonic() - began < args.duration:
                time.sleep(0.5)
            traffic = {}
            traffic_meta = {
                "elapsed_seconds": time.monotonic() - began,
                "workers": 0,
                "peak_active": {op: 0 for op in OPS},
                "peak_total": 0,
                "summary": {},
                "total_ok": 0,
                "total_attempts": 0,
                "total_rps": 0,
            }
        elapsed = traffic_meta["elapsed_seconds"]
        stop.set()
        sampler_thread.join(timeout=3)
        result["traffic"] = traffic
        result["traffic_meta"] = traffic_meta
        log("%s window done in %.1fs, %d ops, %.1f ops/s" % (
            args.traffic, elapsed, traffic_meta["total_ok"], traffic_meta["total_rps"]))

        daemon_rss = [sample["daemon"]["rss"] for sample in samples if sample["daemon"]]
        tree_rss = [sample["tree"]["rss"] for sample in samples if sample["tree"]]
        spk_rss = [
            max(0, sample["tree"]["rss"] - sample["daemon"]["rss"])
            for sample in samples if sample["daemon"] and sample["tree"]
        ]
        tree_pss = [
            sample["tree"].get("pss", 0) for sample in samples if sample["tree"]
        ]
        processes = [sample["tree"]["processes"] for sample in samples if sample["tree"]]
        daemon_cpu = [sample["daemon"]["cpu"] for sample in samples if sample["daemon"]]
        tree_cpu = [sample["tree"]["cpu"] for sample in samples if sample["tree"]]
        daemon_threads = [sample["daemon"]["threads"] for sample in samples if sample["daemon"]]
        tree_threads = [sample["tree"]["threads"] for sample in samples if sample["tree"]]
        result["resources"] = {
            "samples": len(samples),
            "daemon_rss": summarize(daemon_rss),
            "spk_rss": summarize(spk_rss),
            "tree_rss": summarize(tree_rss),
            "tree_pss": summarize(tree_pss),
            "daemon_threads": summarize(daemon_threads),
            "tree_threads": summarize(tree_threads),
            "processes": summarize(processes),
            "daemon_cpu_percent": (
                (daemon_cpu[-1] - daemon_cpu[0]) / elapsed * 100 if len(daemon_cpu) >= 2 else 0
            ),
            "tree_cpu_percent": (
                (tree_cpu[-1] - tree_cpu[0]) / elapsed * 100 if len(tree_cpu) >= 2 else 0
            ),
        }
        log("daemon RSS %.1f/%.1f MB, spk RSS %.1f/%.1f MB, tree %.1f/%.1f MB, threads %d/%d" % (
            result["resources"]["daemon_rss"]["avg"] / MIB,
            result["resources"]["daemon_rss"]["max"] / MIB,
            result["resources"]["spk_rss"]["avg"] / MIB,
            result["resources"]["spk_rss"]["max"] / MIB,
            result["resources"]["tree_rss"]["avg"] / MIB,
            result["resources"]["tree_rss"]["max"] / MIB,
            result["resources"]["daemon_threads"]["max"],
            result["resources"]["tree_threads"]["max"],
        ))

        recovery_ok = True
        if args.traffic == "all":
            if not args.no_oversubscribe:
                outcomes = oversubscribe(daemon["address"], plugins)
                result["oversubscription"]["sse"] = outcomes["sse"]
                result["oversubscription"]["ws"] = outcomes["ws"]
                log("oversubscription sse=%s ws=%s" % (outcomes["sse"], outcomes["ws"]))
            recovery_ok, recovery_status, _ = op_invoke(
                daemon["address"], daemon["token"], plugins[0], 0
            )
            result["oversubscription"]["recovery"] = {
                "ok": recovery_ok, "status": recovery_status,
            }

        if args.bootstrap == "direct":
            running = len(live_plugins(daemon, plugins))
            services_http = None
        else:
            services_http, _, body = http_request(
                daemon["address"], "GET", "/api/v1/services", {}, None
            )
            services = (read_json(body) or {}).get("services") or []
            states = {}
            for entry in services:
                identity, state = service_identity(entry)
                if identity:
                    states[identity] = state
            running = sum(
                1 for plugin in plugins if str(states.get(plugin, "")).lower() == "running"
            )
        strict_coverage = None
        if args.traffic == "all":
            strict_coverage = all(
                traffic[plugin][op]["ok"] > 0
                for plugin in plugins for op in OPS
            )
        stress = args.per_plugin_concurrency > 0
        if args.traffic == "idle":
            coverage = True
        elif stress:
            # Above the budget's derived caps the point is not "every plugin got
            # every surface" but "the daemon stayed up and kept serving whatever
            # the limits admitted".
            coverage = traffic_meta["total_ok"] > 0
        else:
            coverage = strict_coverage
        status_counts = count_statuses(traffic)
        health = {
            "daemon_alive": daemon["process"].poll() is None,
            "services_http": services_http,
            "running": running,
            "expected": len(plugins),
            "coverage": coverage,
            "coverage_strict": strict_coverage,
            "stress": stress,
            "statuses": status_counts,
            "recovery_ok": recovery_ok,
        }
        result["health"] = health
        log("health alive=%s running=%d/%d coverage=%s strict=%s" % (
            health["daemon_alive"], running, len(plugins), coverage, strict_coverage))

        # 503 is the expected refusal when the requested concurrency exceeds the
        # derived caps; 500 or a crash is not.
        server_errors = [
            code for code in status_counts
            if code.startswith("5") and (not stress or code != "503")
        ]
        ok = (
            health["daemon_alive"]
            and running == len(plugins)
            and coverage
            and recovery_ok
            and not server_errors
        )
        exit_code = 0 if ok else 1
    finally:
        stop.set()
        if daemon is not None:
            stop_daemon(root, daemon)
        out = Path(args.out)
        if not out.is_absolute():
            out = repo / out
        report = Path(args.report)
        if not report.is_absolute():
            report = repo / report
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        report.write_text(render_report(result) + "\n", encoding="utf-8")
        log("results %s" % out)
        log("report %s" % report)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
