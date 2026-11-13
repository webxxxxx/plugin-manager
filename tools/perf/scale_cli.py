#!/usr/bin/env python3
"""Measure daemon RSS/CPU with the CLI only (no HTTP API).

Used to compare the default build with --no-default-features on Linux/WSL.
The driver creates a minimal sh plugin, installs/starts N copies through the
svcd control channel, and samples /proc.
"""
import argparse
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

CLK_TCK = os.sysconf("SC_CLK_TCK")


def run(cmd, env, cwd=None, timeout=30):
    return subprocess.run(cmd, env=env, cwd=cwd, check=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, timeout=timeout)


def proc_info(pid):
    try:
        status = pathlib.Path(f"/proc/{pid}/status").read_text()
        stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    rss = 0
    vm = 0
    for line in status.splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1]) * 1024
        elif line.startswith("VmSize:"):
            vm = int(line.split()[1]) * 1024
    close = stat.rfind(")")
    fields = stat[close + 2:].split()
    cpu = (int(fields[11]) + int(fields[12])) / CLK_TCK
    return {"pid": pid, "rss": rss, "vm": vm, "cpu": cpu}


def process_tree(pid):
    table = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        info = proc_info(int(entry))
        if info is None:
            continue
        try:
            stat = pathlib.Path(f"/proc/{entry}/stat").read_text()
        except OSError:
            continue
        close = stat.rfind(")")
        fields = stat[close + 2:].split()
        table[int(entry)] = {
            "ppid": int(fields[1]),
            "rss": info["rss"],
            "vm": info["vm"],
            "cpu": info["cpu"],
        }
    seen = {pid}
    queue = [pid]
    while queue:
        parent = queue.pop(0)
        for child, info in table.items():
            if info["ppid"] == parent and child not in seen:
                seen.add(child)
                queue.append(child)
    total = {"count": 0, "rss": 0, "vm": 0, "cpu": 0.0}
    for child in seen:
        info = table.get(child)
        if not info:
            continue
        total["count"] += 1
        total["rss"] += info["rss"]
        total["vm"] += info["vm"]
        total["cpu"] += info["cpu"]
    return total


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--work", required=True)
    parser.add_argument("--plugins", default="0,30")
    parser.add_argument("--key", required=True)
    parser.add_argument("--pub", required=True)
    parser.add_argument("--window", type=float, default=5.0)
    args = parser.parse_args()

    work = pathlib.Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    bindir = work / "bin"
    bindir.mkdir(exist_ok=True)
    for name in ("svcd", "plugin", "svc"):
        target = bindir / name
        shutil.copyfile(args.binary, target)
        target.chmod(0o755)

    config = work / "config"
    data = work / "data"
    logs = work / "logs"
    trust = config / "trust.d"
    for path in (config, data, logs, trust):
        path.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(args.pub, trust / "sample-2026.pub")

    env = os.environ.copy()
    env.update({
        "SVCD_CONFIG_DIR": str(config),
        "SVCD_DATA_DIR": str(data),
        "SVCD_LOG_DIR": str(logs),
        "SVCD_TRUST_DIR": str(trust),
        "SVCD_HTTP_PORT": "0",
    })

    fixture = work / "plugin"
    (fixture / "payload" / "bin").mkdir(parents=True, exist_ok=True)
    script = fixture / "payload" / "bin" / "agent.sh"
    script.write_text('#!/bin/sh\nwhile IFS= read -r line; do\n  printf \'{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\\n\'\ndone\n')
    script.chmod(0o755)
    (fixture / "manifest.json").write_text(json.dumps({
        "schema": 1, "name": "perf", "version": "1.0.0",
        "desc": "minimal script plugin", "protocol": 1,
        "exec": {"path": "payload/bin/agent.sh", "ready": "notify"},
        "capabilities": [],
    }, indent=2) + "\n")

    daemon_log = open(work / "daemon.stderr", "ab")
    daemon = subprocess.Popen([str(bindir / "svcd"), "run", "--foreground"],
                              env=env, cwd=str(work), stdout=daemon_log,
                              stderr=daemon_log, start_new_session=True)
    try:
        deadline = time.time() + 10
        ready = False
        while time.time() < deadline:
            try:
                run([str(bindir / "svc"), "list"], env, timeout=2)
                ready = True
                break
            except Exception:
                time.sleep(0.1)
        if not ready:
            raise RuntimeError("daemon control channel did not become ready")

        counts = [int(value) for value in args.plugins.split(",") if value.strip()]
        results = []
        for count in counts:
            start = time.time()
            for index in range(count):
                plugin_id = f"p{index}"
                spk = work / f"{plugin_id}.spk"
                run([str(bindir / "plugin"), "bundle", str(fixture),
                     "--id", plugin_id, "--key", args.key,
                     "--key-id", "sample-2026", "-o", str(spk)], env)
                run([str(bindir / "plugin"), "install", str(spk)], env)
                run([str(bindir / "svc"), "start", plugin_id], env, timeout=30)
            start_ms = (time.time() - start) * 1000
            time.sleep(2)
            before_daemon = proc_info(daemon.pid)
            before_tree = process_tree(daemon.pid)
            time.sleep(args.window)
            after_daemon = proc_info(daemon.pid)
            after_tree = process_tree(daemon.pid)
            daemon_cpu = (after_daemon["cpu"] - before_daemon["cpu"]) / args.window * 100
            plugin_cpu = (after_tree["cpu"] - after_daemon["cpu"] -
                          (before_tree["cpu"] - before_daemon["cpu"])) / args.window * 100
            results.append({
                "plugins": count,
                "processes": after_tree["count"],
                "daemon_rss": after_daemon["rss"],
                "daemon_vm": after_daemon["vm"],
                "plugin_rss": after_tree["rss"] - after_daemon["rss"],
                "total_rss": after_tree["rss"],
                "total_vm": after_tree["vm"],
                "daemon_cpu_percent": daemon_cpu,
                "plugin_cpu_percent": plugin_cpu,
                "start_ms": start_ms,
            })
            print(json.dumps({"plugins": count, "daemon_rss_mb": round(after_daemon["rss"] / 1048576, 2),
                              "daemon_vm_mb": round(after_daemon["vm"] / 1048576, 1),
                              "plugin_rss_mb": round((after_tree["rss"] - after_daemon["rss"]) / 1048576, 2),
                              "daemon_cpu_percent": round(daemon_cpu, 2),
                              "start_ms": round(start_ms)}, ensure_ascii=False), flush=True)
        (work / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    finally:
        try:
            subprocess.run([str(bindir / "svcd"), "shutdown"], env=env,
                           timeout=5, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except Exception:
            daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except Exception:
            daemon.kill()


if __name__ == "__main__":
    main()
