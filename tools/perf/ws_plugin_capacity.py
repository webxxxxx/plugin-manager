#!/usr/bin/env python3
"""WebSocket plugin capacity: one held WS connection per plugin, per budget tier.

Answers one question. A WebSocket plugin is installed N times and every copy
holds exactly one WebSocket connection (single concurrency, no other surface in
use). How many run at the same time before the daemon refuses a connection or a
call fails -- under the 50 MB, 512 MB and 2 GB budgets?

The socket is the route a plugin web UI opens itself:
    GET /plugin-ui/<id>/<token>/ws
with the token minted by POST /api/v1/plugins/<id>/ui-token. Calls travel as
tools/call frames on the same socket and the plugin answer comes back on it, so
a held connection is proven alive by real round trips, not by an open fd.

The default fixture is the bash NDJSON agent the five-surface budget scenario
uses (tools/perf/budget_scenario.py), so the numbers stay comparable. Pass
--fixture samples/spk-tests/ws-panel --capability status to drive the packaged
WebSocket sample instead (it also ticks once per second on its own).

Usage:
  python3 tools/perf/ws_plugin_capacity.py --binary /tmp/target/release/svcd \
    --budget 50mb --plugins 288 --duration 20 --work /tmp/ws-50m-288 \
    --out .scratch/ws/50m-288.json --report .scratch/ws/50m-288.md
"""

import argparse
import json
import os
import selectors
import shutil
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import budget_scenario as bs  # noqa: E402 - path first, then the sibling module
from budget_scenario import MIB, log, parse_size  # noqa: E402

CALL_TIMEOUT = 5.0
MASK = 0x80
TEXT, PING, PONG, CLOSE = 0x1, 0x9, 0xA, 0x8


def encode_frame(opcode, payload):
    """One masked client frame; everything this scenario sends is a client frame."""
    header = bytearray([MASK | opcode])
    length = len(payload)
    if length < 126:
        header.append(0x80 | length)
    elif length < 65536:
        header.append(0x80 | 126)
        header += struct.pack("!H", length)
    else:
        header.append(0x80 | 127)
        header += struct.pack("!Q", length)
    mask = os.urandom(4)
    header += mask
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    return bytes(header) + masked


def decode_frames(buffer):
    """Split what arrived into whole frames; a partial tail stays buffered."""
    frames = []
    while len(buffer) >= 2:
        opcode = buffer[0] & 0x0F
        masked = buffer[1] & 0x80
        length = buffer[1] & 0x7F
        offset = 2
        if length == 126:
            if len(buffer) < 4:
                break
            length = struct.unpack("!H", buffer[2:4])[0]
            offset = 4
        elif length == 127:
            if len(buffer) < 10:
                break
            length = struct.unpack("!Q", buffer[2:10])[0]
            offset = 10
        mask = None
        if masked:
            if len(buffer) < offset + 4:
                break
            mask = buffer[offset:offset + 4]
            offset += 4
        if len(buffer) < offset + length:
            break
        payload = buffer[offset:offset + length]
        if mask:
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        frames.append((opcode, payload))
        buffer = buffer[offset + length:]
    return frames, buffer


class Link:
    """One plugin socket: the held connection, its one in-flight call, counters."""

    __slots__ = (
        "plugin", "sock", "buffer", "pending", "pending_at", "next_send",
        "calls", "ok", "errors", "timeouts", "notifications", "latencies", "closed",
    )

    def __init__(self, plugin, sock, next_send):
        self.plugin = plugin
        self.sock = sock
        self.buffer = b""
        self.pending = None
        self.pending_at = 0.0
        self.next_send = next_send
        self.calls = 0
        self.ok = 0
        self.errors = 0
        self.timeouts = 0
        self.notifications = 0
        self.latencies = []
        self.closed = None


def open_links(address, plugins, progress=0):
    """Mint one token, open one socket, move to the next plugin.

    The token is minted right before its own handshake because the daemon keeps
    at most 256 plugin page tokens and evicts the oldest; minting all of them up
    front would leave most plugins unable to handshake at all.
    """
    started = time.monotonic()
    links = {}
    refused = {}
    tokens = {}
    for index, plugin in enumerate(plugins):
        try:
            token = bs.plugin_token(address, plugin, tokens)
        except Exception as error:  # noqa: BLE001 - a refusal is data
            refused[plugin] = "token:%s" % str(error)[:80]
            continue
        try:
            sock, status = bs._ws_open(address, plugin, token, timeout=15.0)
        except Exception as error:  # noqa: BLE001 - a refusal is data
            refused[plugin] = "handshake:%s" % type(error).__name__
            continue
        if sock is None:
            refused[plugin] = "http-%s" % status
            continue
        sock.setblocking(False)
        links[plugin] = Link(plugin, sock, time.monotonic())
        if progress and (index + 1) % progress == 0:
            log("opened %d/%d (%.1fs)" % (len(links), len(plugins), time.monotonic() - started))
    return links, refused


def close_link(selector, link, reason):
    if link.closed is not None:
        return
    link.closed = reason
    try:
        selector.unregister(link.sock)
    except Exception:
        pass
    try:
        link.sock.close()
    except Exception:
        pass


def hold(capability, links, duration, interval_ms):
    """Keep every socket open, one call in flight per plugin, for the whole window.

    Returns how many connections were still open when the window ended; the
    sockets are closed right after that, so a reader of the counters cannot
    mistake the teardown for a drop.
    """
    alive = 0
    selector = selectors.DefaultSelector()
    for link in links.values():
        selector.register(link.sock, selectors.EVENT_READ, link)
    deadline = time.monotonic() + duration
    sequence = 0
    try:
        while time.monotonic() < deadline:
            now = time.monotonic()
            for link in links.values():
                if link.closed is not None:
                    continue
                if link.pending is not None:
                    if now - link.pending_at > CALL_TIMEOUT:
                        link.timeouts += 1
                        link.errors += 1
                        link.pending = None
                        link.next_send = now
                    continue
                if now < link.next_send:
                    continue
                sequence += 1
                request_id = 500000 + sequence
                frame = encode_frame(TEXT, json.dumps({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": "tools/call",
                    "params": {"name": link.plugin + "." + capability, "arguments": {}},
                }).encode("utf-8"))
                try:
                    link.sock.sendall(frame)
                except BlockingIOError:
                    continue
                except OSError as error:
                    link.errors += 1
                    close_link(selector, link, "send:%s" % type(error).__name__)
                    continue
                link.calls += 1
                link.pending = request_id
                link.pending_at = now
            for key, _ in selector.select(0.02):
                link = key.data
                if link.closed is not None:
                    continue
                try:
                    chunk = link.sock.recv(65536)
                except BlockingIOError:
                    continue
                except OSError as error:
                    link.errors += 1
                    close_link(selector, link, "recv:%s" % type(error).__name__)
                    continue
                if not chunk:
                    link.errors += 1
                    close_link(selector, link, "eof")
                    continue
                link.buffer += chunk
                frames, link.buffer = decode_frames(link.buffer)
                for opcode, payload in frames:
                    if opcode == CLOSE:
                        close_link(selector, link, "close-frame")
                        break
                    if opcode == PING:
                        try:
                            link.sock.sendall(encode_frame(PONG, payload))
                        except OSError:
                            pass
                        continue
                    if opcode != TEXT:
                        continue
                    try:
                        message = json.loads(payload.decode("utf-8", "replace"))
                    except ValueError:
                        link.errors += 1
                        continue
                    # Only a frame whose id is still in flight answers the call.
                    # Once the reply is in, link.pending is None and every later
                    # frame (plugin notifications, already-answered duplicates)
                    # is traffic on the same held socket.
                    if link.pending is None or message.get("id") != link.pending:
                        link.notifications += 1
                        continue
                    elapsed = time.monotonic() - link.pending_at
                    link.pending = None
                    link.next_send = time.monotonic() + interval_ms / 1000.0
                    result = message.get("result") or {}
                    if "error" in message or result.get("isError"):
                        link.errors += 1
                    else:
                        link.ok += 1
                        link.latencies.append(elapsed * 1000.0)
        alive = sum(1 for link in links.values() if link.closed is None)
    finally:
        for link in links.values():
            close_link(selector, link, "run-end")
        selector.close()
    return alive


def summarize(links, refused, requested, duration, alive_at_end):
    calls = sum(link.calls for link in links.values())
    ok = sum(link.ok for link in links.values())
    errors = sum(link.errors for link in links.values())
    timeouts = sum(link.timeouts for link in links.values())
    notifications = sum(link.notifications for link in links.values())
    latencies = [value for link in links.values() for value in link.latencies]
    buckets = {}
    for detail in refused.values():
        key = detail.split(":", 1)[0]
        buckets[key] = buckets.get(key, 0) + 1
    dropped = {
        plugin: link.closed for plugin, link in links.items()
        if link.closed not in (None, "run-end")
    }
    silent = [plugin for plugin, link in links.items() if link.ok == 0 and not link.closed]
    return {
        "requested": requested,
        "opened": len(links),
        "refused": len(refused),
        "refused_buckets": buckets,
        "calls": calls,
        "ok": ok,
        "errors": errors,
        "timeouts": timeouts,
        "notifications": notifications,
        "dropped": dropped,
        "silent": silent,
        "duration": duration,
        "rps": (calls / duration) if duration else 0,
        "latency": bs.summarize(latencies),
        "alive_at_end": alive_at_end,
        "saturated": (not refused) and errors == 0 and ok > 0 and not silent,
    }


def sample_loop(daemon, stop, samples):
    while not stop.wait(2.0):
        try:
            daemon_sample = bs.sample_process(daemon["process"].pid)
            tree_sample = bs.process_tree(daemon["process"].pid)
        except Exception as error:  # noqa: BLE001 - one vanished child must not end the run
            log("sample skipped: %r" % (error,))
            continue
        samples.append({"at": time.time(), "daemon": daemon_sample, "tree": tree_sample})


def resource_report(samples):
    daemon_rss = [item["daemon"]["rss"] for item in samples if item["daemon"]]
    tree_rss = [item["tree"]["rss"] for item in samples if item["tree"]]
    tree_pss = [item["tree"].get("pss", 0) for item in samples if item["tree"]]
    threads = [item["daemon"]["threads"] for item in samples if item["daemon"]]
    daemon_cpu = [item["daemon"]["cpu"] for item in samples if item["daemon"]]
    processes = [item["tree"]["processes"] for item in samples if item["tree"]]
    return {
        "samples": len(samples),
        "daemon_rss_max": max(daemon_rss) if daemon_rss else 0,
        "daemon_rss_last": daemon_rss[-1] if daemon_rss else 0,
        "tree_rss_max": max(tree_rss) if tree_rss else 0,
        "tree_pss_max": max(tree_pss) if tree_pss else 0,
        "daemon_threads_max": max(threads) if threads else 0,
        "daemon_cpu_max": max(daemon_cpu) if daemon_cpu else 0,
        "processes_max": max(processes) if processes else 0,
    }


def render(result):
    requested = result["requested"]
    connections = result["connections"] or {}
    resources = result["resources"]
    effective = result["effective"] or {}
    limits = result["limits"]
    budget = "%d%%" % requested["budget_percent"] if requested["budget_percent"] else "%g MB" % (
        requested["budget_bytes"] / MIB)
    verdict = result["verdict"]
    latency = connections.get("latency") or {}
    lines = [
        "# WebSocket plugin capacity: %s, %d plugins" % (budget, requested["plugins"]),
        "",
        "| item | value |",
        "| --- | ---: |",
        "| requested budget | %s |" % budget,
        "| effective budget | %s |" % effective.get("budget_quantized"),
        "| workload | %s |" % effective.get("workload"),
        "| max_connections / max_streams | %s / %s |" % (
            limits.get("max_connections"), limits.get("max_streams")),
        "| plugins | %d |" % requested["plugins"],
        "| connections opened | %s |" % connections.get("opened"),
        "| connections refused | %s |" % connections.get("refused"),
        "| calls ok / calls | %s / %s |" % (connections.get("ok"), connections.get("calls")),
        "| errors / timeouts | %s / %s |" % (connections.get("errors"), connections.get("timeouts")),
        "| alive at end | %s |" % connections.get("alive_at_end"),
        "| notifications | %s |" % connections.get("notifications"),
        "| p50 / p99 latency | %.2f ms / %.2f ms |" % (latency.get("p50", 0), latency.get("p99", 0)),
        "| daemon RSS max | %.1f MB |" % (resources.get("daemon_rss_max", 0) / MIB),
        "| tree PSS max | %.1f MB |" % (resources.get("tree_pss_max", 0) / MIB),
        "| tree RSS max | %.1f MB |" % (resources.get("tree_rss_max", 0) / MIB),
        "| daemon threads max | %s |" % resources.get("daemon_threads_max"),
        "| processes max | %s |" % resources.get("processes_max"),
        "| verdict | %s |" % ("100% OK" if verdict.get("ok") else "FAILED: " + verdict.get("reason", "")),
        "",
    ]
    refusals = connections.get("refusals") or {}
    if refusals:
        lines += ["## Refusals", "", "| plugin | detail |", "| --- | --- |"]
        lines += ["| %s | %s |" % (plugin, detail) for plugin, detail in refusals.items()]
        lines.append("")
    dropped = connections.get("dropped") or {}
    if dropped:
        lines += ["## Dropped", "", "| plugin | reason |", "| --- | --- |"]
        lines += ["| %s | %s |" % (plugin, reason) for plugin, reason in list(dropped.items())[:40]]
        lines.append("")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", default=os.path.join("target", "release", "svcd" + bs.EXE))
    parser.add_argument("--budget", type=parse_size, default=parse_size("50mb"))
    parser.add_argument("--budget-percent", type=int, default=0)
    parser.add_argument("--workload", choices=("", "tiny", "interactive", "streaming"), default="")
    parser.add_argument("--plugins", type=int, default=10)
    parser.add_argument("--duration", type=float, default=20.0)
    parser.add_argument("--think-ms", type=float, default=1000.0,
                        help="pause between two calls on the same connection")
    parser.add_argument("--cpu-budget-percent", type=int, default=90)
    parser.add_argument("--bootstrap", choices=("api", "direct"), default="direct")
    parser.add_argument("--plugin-runtime", choices=("shell", "python"), default="shell")
    parser.add_argument("--fixture", default=None,
                        help="plugin directory to install instead of the generated fixture")
    parser.add_argument("--capability", default="echo", help="capability the held call invokes")
    parser.add_argument("--key", default=os.path.join("samples", "keys", "sample-2026.key"))
    parser.add_argument("--pub", default=os.path.join("samples", "keys", "sample-2026.pub"))
    parser.add_argument("--config", action="append", default=[], metavar="KEY=VALUE",
                        help="extra svcd.conf line (repeatable), e.g. max_complete_body_bytes=8192")
    parser.add_argument("--work", default=None)
    parser.add_argument("--out", default=None)
    parser.add_argument("--report", default=None)
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
        raise SystemExit("missing sample keys under %s" % (repo / "samples" / "keys"))

    root = Path(args.work) if args.work else repo / "target" / "ws-capacity"
    root.mkdir(parents=True, exist_ok=True)
    plugins = ["p%03d" % index for index in range(args.plugins)]
    log("workdir %s, %d plugins, %ss, budget %s" % (
        root, args.plugins, args.duration,
        "%d%%" % args.budget_percent if args.budget_percent else "%gmb" % (args.budget / MIB)))

    result = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "workdir": str(root),
        "environment": {"os": os.uname().sysname + " " + os.uname().machine,
                        "cpus": os.cpu_count() or 1},
        "overrides": list(args.config),
        "requested": {
            "budget_bytes": args.budget,
            "budget_percent": args.budget_percent,
            "workload": args.workload,
            "plugins": args.plugins,
            "duration": args.duration,
            "think_ms": args.think_ms,
            "cpu_budget_percent": args.cpu_budget_percent,
            "fixture": args.fixture or args.plugin_runtime,
            "capability": args.capability,
        },
        "effective": None,
        "limits": {},
        "connections": {},
        "resources": {},
        "verdict": {},
    }
    exit_code = 1
    daemon = None
    samples = []
    stop = threading.Event()
    try:
        if args.fixture:
            fixture = Path(args.fixture)
            if not fixture.is_absolute():
                fixture = repo / fixture
            if not fixture.is_dir():
                raise SystemExit("missing fixture: %s" % fixture)
        else:
            fixture = bs.write_fixture(root, args.plugin_runtime)
        if args.bootstrap == "direct":
            target = bs.detect_platform()
            bs.write_direct_layout(root, fixture, plugins, target)
            log("direct layout for %d plugins (platform %s)" % (len(plugins), target))
        daemon = bs.start_daemon(
            root, binary, args.budget, pub, args.cpu_budget_percent,
            args.budget_percent, args.workload, extra=args.config)
        log("daemon on %s, pid %d" % (daemon["address"], daemon["process"].pid))
        result["effective"] = bs.startup_report(root)
        svc = root / "bin" / ("svc" + bs.EXE)
        shutil.copyfile(binary, svc)
        if not bs.WIN:
            svc.chmod(0o755)
        tune = subprocess.run(
            [str(svc), "config", "tune"], env=daemon["env"], cwd=str(root),
            capture_output=True, text=True, timeout=30)
        result["limits"] = bs.parse_tune(tune.stdout)
        log("limits: connections=%s streams=%s" % (
            result["limits"].get("max_connections"), result["limits"].get("max_streams")))

        if args.bootstrap == "direct":
            live = bs.wait_for_live(daemon, plugins, timeout=max(60.0, 0.25 * len(plugins)))
            log("%d/%d plugins live" % (len(live), len(plugins)))
            if len(live) != len(plugins):
                missing = [plugin for plugin in plugins if plugin not in live]
                raise RuntimeError("%d plugins did not come up: %s" % (len(missing), missing[:8]))
        else:
            packages = bs.bundle_plugins(root, binary, daemon["env"], fixture, plugins, key)
            bs.install_and_start(daemon["address"], packages, plugins)

        sampler = threading.Thread(target=sample_loop, args=(daemon, stop, samples), daemon=True)
        sampler.start()

        began = time.monotonic()
        links, refused = open_links(daemon["address"], plugins,
                                    progress=max(1, len(plugins) // 8))
        opened_after = time.monotonic() - began
        log("opened %d, refused %d (%.1fs)" % (len(links), len(refused), opened_after))
        if refused:
            log("refusals: %s" % json.dumps(dict(list(refused.items())[:8])))

        alive_at_end = hold(args.capability, links, args.duration, args.think_ms)
        connections = summarize(links, refused, args.plugins, args.duration, alive_at_end)
        connections["open_seconds"] = opened_after
        connections["refusals"] = dict(sorted(refused.items())[:40])
        result["connections"] = connections

        stop.set()
        sampler.join(timeout=3)
        result["resources"] = resource_report(samples)
        result["verdict"] = {
            "ok": bool(connections["saturated"]),
            "reason": (
                "all %d connections opened and every call succeeded" % args.plugins
                if connections["saturated"] else
                "refused=%d errors=%d timeouts=%d silent=%d dropped=%d" % (
                    connections["refused"], connections["errors"], connections["timeouts"],
                    len(connections["silent"]), len(connections["dropped"]))
            ),
        }
        log("calls %d ok %d errors %d timeouts %d, %.1f calls/s, alive %d/%d" % (
            connections["calls"], connections["ok"], connections["errors"],
            connections["timeouts"], connections["rps"],
            connections["alive_at_end"], len(plugins)))
        log("daemon RSS max %.1f MB, tree PSS max %.1f MB, threads %d, processes %d" % (
            result["resources"]["daemon_rss_max"] / MIB,
            result["resources"]["tree_pss_max"] / MIB,
            result["resources"]["daemon_threads_max"],
            result["resources"]["processes_max"]))
        log("verdict: %s" % result["verdict"]["reason"])
        exit_code = 0 if connections["saturated"] else 2
    finally:
        stop.set()
        if daemon is not None:
            bs.stop_daemon(root, daemon)
        out = Path(args.out) if args.out else root / "ws-connections.json"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        log("wrote %s" % out)
        report = Path(args.report) if args.report else root / "ws-connections.md"
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(render(result), encoding="utf-8")
        log("wrote %s" % report)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
