#!/usr/bin/env python3
"""Measure an idle daemon's RSS/VM/threads/processes for several plugin counts.

Usage (WSL/Linux):
  python3 tools/perf/memory_by_plugins.py \
    --binary target/release/svcd --counts 0,3,10
"""

import argparse
import json
import pathlib
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import reactor_scale  # noqa: E402


def read_status(pid):
    values = {}
    for line in pathlib.Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith(("VmRSS:", "VmSize:", "Threads:")):
            key, value = line.split(":", 1)
            values[key] = int(value.split()[0])
    return values


def children(pid):
    try:
        text = pathlib.Path(f"/proc/{pid}/task/{pid}/children").read_text()
        return [int(value) for value in text.split()]
    except (OSError, ValueError):
        return []


def descendants(pid):
    found = []
    pending = children(pid)
    while pending:
        child = pending.pop()
        if child in found:
            continue
        found.append(child)
        pending.extend(children(child))
    return found


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/svcd")
    parser.add_argument("--work", default="/tmp/svcd-memory-by-plugins")
    parser.add_argument("--counts", default="0,3,10")
    parser.add_argument("--key", default="samples/keys/sample-2026.key")
    parser.add_argument("--pub", default="samples/keys/sample-2026.pub")
    parser.add_argument("--settle", type=float, default=3.0)
    args = parser.parse_args()

    binary = pathlib.Path(args.binary).resolve()
    key = pathlib.Path(args.key).resolve()
    pub = pathlib.Path(args.pub).resolve()
    results = []
    for count in [int(value) for value in args.counts.split(",") if value]:
        root = pathlib.Path(f"{args.work}-{count}").resolve()
        env = reactor_scale.setup(root, binary, key, pub)
        fixture = reactor_scale.make_fixture(root)
        daemon = subprocess.Popen(
            [str(root / "bin" / "svcd"), "run", "--foreground"],
            env=env, stdout=(root / "daemon.log").open("w"),
            stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            reactor_scale.wait_for_http(root / "config", daemon)
            if count:
                reactor_scale.install_plugins(
                    root, env, fixture, count, key
                )
            time.sleep(args.settle)
            status = read_status(daemon.pid)
            processes = 1 + len(descendants(daemon.pid))
            results.append({
                "plugins": count,
                "rss_kb": status.get("VmRSS"),
                "vm_kb": status.get("VmSize"),
                "threads": status.get("Threads"),
                "processes": processes,
            })
        finally:
            try:
                subprocess.run(
                    [str(root / "bin" / "svcd"), "shutdown"], env=env,
                    timeout=5, stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            except Exception:
                daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except Exception:
                daemon.kill()
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
