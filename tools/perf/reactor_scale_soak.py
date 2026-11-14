#!/usr/bin/env python3
"""3-minute soak test: 20 plugins, 1000 client threads, mixed reactor traffic.

Cycles HTTP -> MCP -> SSE -> WS with 1000 OS client threads and records the
daemon's CPU, RSS/VM, thread count and process tree for the whole duration.

Usage:
  python3 tools/perf/reactor_scale_soak.py \
    --binary target/release/svcd --work /tmp/svcd-reactor-soak \
    --plugins 20 --threads 1000 --duration 180
"""

import argparse
import concurrent.futures
import json
import pathlib
import subprocess
import sys
import threading
import time

import reactor_scale
import reactor_scale_threads as rt

PHASES = ("http", "mcp", "sse", "ws")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/svcd")
    parser.add_argument("--work", default="/tmp/svcd-reactor-soak")
    parser.add_argument("--plugins", type=int, default=20)
    parser.add_argument("--threads", type=int, default=1000)
    parser.add_argument("--duration", type=float, default=180.0)
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
    metrics = rt.Metrics(daemon.pid)
    try:
        address = reactor_scale.wait_for_http(root / "config", daemon)
        reactor_scale.install_plugins(root, env, fixture, args.plugins, key)
        time.sleep(0.5)
        started = time.monotonic()
        deadline = started + args.duration
        totals = {
            phase: {"attempted": 0, "ok": 0, "failed": 0, "phase_seconds": 0.0}
            for phase in PHASES
        }
        cycles = 0

        def tasks_for(phase):
            return [
                (f"p{i % args.plugins}", i)
                for i in range(args.threads)
            ]

        def run_one(phase, task):
            plugin = task[0]
            if phase == "http":
                return rt.http_get(address, f"/plugin-ui/{plugin}/")
            if phase == "mcp":
                return rt.http_mcp(address, plugin, task[1])
            if phase == "sse":
                return rt.sse_connect(address, plugin, args.hold)
            return rt.ws_connect(address, plugin, args.hold)

        with concurrent.futures.ThreadPoolExecutor(
            max_workers=args.threads
        ) as pool:
            while time.monotonic() < deadline:
                for phase in PHASES:
                    if time.monotonic() >= deadline:
                        break
                    phase_started = time.monotonic()
                    results = list(pool.map(
                        lambda task, phase=phase: (phase, run_one(phase, task)),
                        tasks_for(phase),
                    ))
                    elapsed = time.monotonic() - phase_started
                    ok = 0
                    for _, value in results:
                        if phase in ("http",):
                            ok += 1 if value == 200 else 0
                        elif phase == "mcp":
                            ok += 1 if value[1] else 0
                        elif phase == "sse":
                            ok += 1 if value == 200 else 0
                        else:
                            ok += 1 if value == 101 else 0
                    totals[phase]["attempted"] += len(results)
                    totals[phase]["ok"] += ok
                    totals[phase]["failed"] += len(results) - ok
                    totals[phase]["phase_seconds"] += elapsed
                cycles += 1
                elapsed = time.monotonic() - started
                progress = ", ".join(
                    f"{phase}={totals[phase]['ok']}" for phase in PHASES
                )
                print(
                    f"cycle {cycles}: {elapsed:.0f}s ({progress})",
                    file=sys.stderr,
                    flush=True,
                )

        summary = metrics.summary()
        summary["elapsed_sec"] = round(time.monotonic() - started, 3)
        summary["final"] = metrics.current()
        result = {
            "plugins": args.plugins,
            "client_threads": args.threads,
            "concurrency_per_plugin": max(1, args.threads // args.plugins),
            "duration_requested_sec": args.duration,
            "cycles": cycles,
            "totals": totals,
            "daemon": summary,
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
