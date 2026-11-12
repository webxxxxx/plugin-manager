#!/usr/bin/env python3
"""Size gate for the core binary.

Measures the stripped release binary in decimal bytes, compares it with the
absolute budget and the recorded baseline, and emits a JSON report.

Exit codes: 0 = ok, 1 = over budget, 2 = bad input.
"""
import argparse
import json
import os
import sys

EMBEDDED_LIMIT = 2_000_000
DESKTOP_LIMIT = 30_000_000
DRIFT_WARN = 0.02


def kib(n):
    return n / 1024.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--target", required=True, help="target label, e.g. x86_64-unknown-linux-musl")
    ap.add_argument("--limit", type=int, default=EMBEDDED_LIMIT)
    ap.add_argument("--baseline", default="ci/size-baseline.json")
    ap.add_argument("--report", default="size-report.json")
    ap.add_argument("--update-baseline", action="store_true")
    args = ap.parse_args()

    if not os.path.isfile(args.binary):
        print("::error::binary not found: " + args.binary)
        return 2
    size = os.path.getsize(args.binary)

    baseline = {}
    if os.path.isfile(args.baseline):
        with open(args.baseline, "r", encoding="utf-8") as f:
            baseline = json.load(f)

    prev = baseline.get(args.target)
    drift = None if prev is None else (size - prev) / float(prev) * 100.0

    if args.update_baseline:
        baseline[args.target] = size
        os.makedirs(os.path.dirname(args.baseline) or ".", exist_ok=True)
        with open(args.baseline, "w", encoding="utf-8") as f:
            json.dump(baseline, f, indent=2, sort_keys=True)
            f.write("\n")
        prev = size
        drift = 0.0

    over = size > args.limit
    warn = prev is not None and prev > 0 and size > prev * (1.0 + DRIFT_WARN)

    status = "FAIL" if over else "OK"
    print("target:  " + args.target)
    print("binary:  " + args.binary)
    print("size:    {:,} B ({:.1f} KiB)".format(size, kib(size)))
    print("limit:   {:,} B ({:.1f} KiB)  -> {}".format(args.limit, kib(args.limit), status))
    if prev is None:
        print("baseline: none (record it with --update-baseline)")
    else:
        print("baseline: {:,} B ({:+.1f}%)".format(prev, drift))

    report = {
        "target": args.target,
        "binary": args.binary,
        "bytes": size,
        "kib": round(kib(size), 1),
        "limit": args.limit,
        "baseline": prev,
        "driftPercent": None if drift is None else round(drift, 3),
        "status": status,
    }
    with open(args.report, "w", encoding="utf-8") as f:
        json.dump(report, f, indent=2, sort_keys=True)
        f.write("\n")

    if over:
        print("::error file={}::{} is {:,} B, over the {:,} B budget".format(args.binary, args.target, size, args.limit))
        return 1
    if warn:
        print("::warning file={}::{} grew {:+.1f}% vs baseline ({:,} -> {:,} B); update ci/size-baseline.json if intentional".format(
            args.binary, args.target, drift, prev, size))
    return 0


if __name__ == "__main__":
    sys.exit(main())
