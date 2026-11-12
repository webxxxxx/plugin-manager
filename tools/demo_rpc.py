#!/usr/bin/env python3
"""Drive the sample WireGuard agent over the svcd NDJSON protocol."""
import json
import os
import subprocess
import sys

AGENT = os.path.join('samples', 'wireguard', 'payload', 'bin', 'wg-agent')

REQS = [
    {"jsonrpc": "2.0", "id": 1, "method": "svcd.ping"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "status", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "up", "arguments": {"config": "[Interface]\nListenPort=51820\n[Peer]\nPublicKey=demo"}}},
    {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "status", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 5, "method": "svcd.status"},
    {"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "nope", "arguments": {}}},
    {"jsonrpc": "2.0", "id": 7, "method": "no/such"},
    {"jsonrpc": "2.0", "method": "svcd.shutdown"},
]


def main():
    p = subprocess.Popen(
        [sys.executable, AGENT],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    payload = "".join(json.dumps(r, separators=(",", ":")) + "\n" for r in REQS)
    out, err = p.communicate(payload, timeout=10)
    lines = out.strip().splitlines()
    for line in lines:
        print("<- " + line)
    print("exit: " + str(p.returncode))
    err = err.strip()
    if err:
        print("stderr: " + err.replace("\n", " | "))
    return 0 if p.returncode == 0 and len(lines) == 7 else 1


if __name__ == "__main__":
    sys.exit(main())
