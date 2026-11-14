#!/usr/bin/env python3
"""End-to-end smoke test for the WSL-installable sample .spk packages.

Run this inside WSL (or any Linux with python3) against a Linux build of svcd:

    cargo build --release
    python3 tools/spk_smoke.py --binary target/release/svcd

The script bundles the five packages under `samples/spk-tests/` with
`plugin bundle`, installs them into a throwaway config/data tree, and checks:

  1. package dependencies  — dep-app refuses to start without dep-base, then
                              starts dep-base first once it is installed
  2. MCP                   — modern Streamable HTTP plus the dual-era stdio bridge
  3. WebSocket             — handshake, tools/call, notifications, ping/pong, close
  4. SSE                   — EventSource-style stream plus Last-Event-ID replay

The WebSocket and SSE packages ship a web UI, served by svcd at
`/plugin-ui/ws-panel/` and `/plugin-ui/sse-panel/`; the report prints the URLs
so a browser can watch the live effect.

Only the Python standard library is used, so this runs on a bare Debian WSL.
"""
import argparse
import base64
import http.client
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tarfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SOURCES = os.path.join(ROOT, "samples", "spk-tests")
PACKAGES = ["dep-base", "dep-app", "mcp-echo", "ws-panel", "sse-panel"]
PASSWORD = "spk-smoke"
MCP_VERSION = "2026-07-28"


class SmokeError(Exception):
    pass


def fail(message):
    raise SmokeError(message)


class Report:
    def __init__(self, title):
        self.title = title
        self.rows = []

    def check(self, name, fn):
        started = time.time()
        try:
            detail = fn()
            self.rows.append({"name": name, "ok": True, "detail": detail or "", "ms": int((time.time() - started) * 1000)})
            print("  PASS  %-52s %s" % (name, detail or ""))
        except Exception as error:  # noqa: BLE001 - a smoke failure must not abort the report
            detail = error if isinstance(error, SmokeError) else "%s: %s" % (type(error).__name__, error)
            self.rows.append({"name": name, "ok": False, "detail": str(detail), "ms": int((time.time() - started) * 1000)})
            print("  FAIL  %-52s %s" % (name, detail))

    def passed(self):
        return all(row["ok"] for row in self.rows)

    def to_markdown(self, extra):
        lines = ["# " + self.title, ""]
        for key, value in extra.items():
            lines.append("- **%s**: %s" % (key, value))
        lines += ["", "| # | check | result | detail |", "| ---: | --- | --- | --- |"]
        for index, row in enumerate(self.rows, 1):
            lines.append(
                "| %d | %s | %s | %s |"
                % (index, row["name"], "PASS" if row["ok"] else "FAIL", row["detail"].replace("|", "\\|"))
            )
        lines.append("")
        return "\n".join(lines)


def require(condition, message):
    if not condition:
        fail(message)


def run(command, env, cwd=None, timeout=90):
    return subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=timeout,
    )


def cli(binary, args, env, timeout=90):
    result = run([binary] + args, env, timeout=timeout)
    return result.returncode, result.stdout.strip()


def new_work_root():
    base = os.environ.get("SPK_SMOKE_WORK_ROOT") or "/tmp"
    root = os.path.join(base, "svcd-spk-smoke-%d" % os.getpid())
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(root)
    return root


def is_wsl():
    try:
        with open("/proc/version", "r", encoding="utf-8", errors="replace") as handle:
            text = handle.read().lower()
        return "microsoft" in text or "wsl" in text
    except OSError:
        return False


class Http:
    def __init__(self, port):
        self.port = port

    def request(self, method, path, body=None, headers=None, timeout=10):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            connection.request(method, path, body=body, headers=headers or {})
            response = connection.getresponse()
            data = response.read()
            return response.status, dict(response.getheaders()), data
        finally:
            connection.close()

    def json(self, method, path, payload=None, headers=None):
        body = None
        merged = dict(headers or {})
        if payload is not None:
            body = json.dumps(payload).encode("utf-8")
            merged["content-type"] = "application/json"
        status, response_headers, data = self.request(method, path, body, merged)
        try:
            return status, response_headers, json.loads(data.decode("utf-8"))
        except ValueError:
            return status, response_headers, {"raw": data.decode("utf-8", "replace")}

    def text(self, path):
        status, headers, data = self.request("GET", path)
        return status, headers, data.decode("utf-8", "replace")


def modern_meta():
    return {
        "io.modelcontextprotocol/protocolVersion": MCP_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {},
    }


def mcp_http(http, method, params, name=None, password=PASSWORD):
    params = dict(params or {})
    params["_meta"] = modern_meta()
    headers = {
        "Authorization": "Bearer " + password,
        "MCP-Protocol-Version": MCP_VERSION,
        "Mcp-Method": method,
    }
    if name is not None:
        headers["Mcp-Name"] = name
    status, _, body = http.json(
        "POST",
        "/mcp",
        {"jsonrpc": "2.0", "id": 1, "method": method, "params": params},
        headers,
    )
    return status, body


def tool_names(body):
    tools = body.get("result", {}).get("tools", [])
    return [tool.get("name") for tool in tools if isinstance(tool, dict)]


def tool_text(body):
    """The text of the first content block of a tools/call result."""
    return body["result"]["content"][0]["text"]


class StdioBridge:
    """One long-lived `mcp` stdio child, read line by line."""

    def __init__(self, binary, env):
        self.child = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
            bufsize=1,
        )
        self.next_id = 1

    def send(self, message):
        self.child.stdin.write(json.dumps(message) + "\n")
        self.child.stdin.flush()

    def read(self, timeout=10):
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = self.child.stdout.readline()
            if line:
                return json.loads(line)
            time.sleep(0.01)
        fail("stdio mcp bridge did not answer")

    def call(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        while True:
            response = self.read()
            if response.get("id") == request_id:
                return response

    def close(self):
        try:
            self.child.stdin.close()
        except OSError:
            pass
        try:
            self.child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.child.kill()


class WsClient:
    def __init__(self, port, path, headers=None):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=8)
        key = base64.b64encode(os.urandom(16)).decode("ascii")
        request = (
            "GET %s HTTP/1.1\r\n"
            "Host: 127.0.0.1:%d\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            "Sec-WebSocket-Key: %s\r\n"
            "Sec-WebSocket-Version: 13\r\n" % (path, port, key)
        )
        for name, value in (headers or {}).items():
            request += "%s: %s\r\n" % (name, value)
        request += "\r\n"
        self.sock.sendall(request.encode("ascii"))
        self.head = b""
        while not self.head.endswith(b"\r\n\r\n"):
            chunk = self.sock.recv(1)
            if not chunk:
                fail("WebSocket handshake closed early")
            self.head += chunk
        first = self.head.split(b"\r\n", 1)[0].decode("ascii", "replace")
        require(" 101 " in first + " ", "WebSocket handshake failed: " + first)

    def send_frame(self, opcode, payload):
        if isinstance(payload, str):
            payload = payload.encode("utf-8")
        mask = os.urandom(4)
        frame = bytearray([0x80 | opcode])
        length = len(payload)
        if length < 126:
            frame.append(0x80 | length)
        elif length <= 0xFFFF:
            frame.append(0x80 | 126)
            frame += struct.pack("!H", length)
        else:
            frame.append(0x80 | 127)
            frame += struct.pack("!Q", length)
        frame += mask
        frame += bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.sock.sendall(frame)

    def read_frame(self):
        header = self._read_exact(2)
        opcode = header[0] & 0x0F
        masked = bool(header[1] & 0x80)
        length = header[1] & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_exact(2))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_exact(8))[0]
        mask = self._read_exact(4) if masked else None
        payload = self._read_exact(length)
        if mask:
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        return opcode, payload

    def _read_exact(self, size):
        data = b""
        while len(data) < size:
            chunk = self.sock.recv(size - len(data))
            if not chunk:
                fail("WebSocket closed while reading a frame")
            data += chunk
        return data

    def next_message(self, timeout=6):
        self.sock.settimeout(timeout)
        while True:
            opcode, payload = self.read_frame()
            if opcode == 0x1:
                return json.loads(payload.decode("utf-8"))
            if opcode == 0x8:
                code = struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 0
                return {"__close__": code}
            if opcode == 0x9:
                self.send_frame(0xA, payload)
                continue

    def close(self):
        try:
            self.send_frame(0x8, struct.pack("!H", 1000))
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


class SseClient:
    """Chunked text/event-stream reader with an explicit Last-Event-ID."""

    def __init__(self, port, path, last_event_id=None):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        request = (
            "GET %s HTTP/1.1\r\n"
            "Host: 127.0.0.1:%d\r\n"
            "Accept: text/event-stream\r\n"
            "Connection: close\r\n" % (path, port)
        )
        if last_event_id is not None:
            request += "Last-Event-ID: %d\r\n" % last_event_id
        request += "\r\n"
        self.sock.sendall(request.encode("ascii"))
        self.file = self.sock.makefile("rb")
        status = self.file.readline().decode("ascii", "replace")
        require(" 200 " in status + " ", "SSE status was not 200: " + status.strip())
        self.headers = {}
        while True:
            line = self.file.readline()
            if line in (b"\r\n", b"\n", b""):
                break
            name, _, value = line.decode("ascii", "replace").partition(":")
            self.headers[name.strip().lower()] = value.strip()
        require(
            "text/event-stream" in self.headers.get("content-type", ""),
            "SSE content-type is not text/event-stream: %r" % self.headers,
        )
        self.buffer = b""

    def _chunk(self):
        size_line = self.file.readline()
        if not size_line:
            fail("SSE stream closed")
        size = int(size_line.strip().split(b";")[0], 16)
        if size == 0:
            while True:
                trailer = self.file.readline()
                if trailer in (b"\r\n", b"\n", b""):
                    raise EOFError
        data = self.file.read(size)
        self.file.read(2)
        return data

    def next_event(self, timeout=8):
        self.sock.settimeout(timeout)
        while True:
            index = self.buffer.find(b"\n\n")
            if index >= 0:
                block = self.buffer[:index].decode("utf-8", "replace")
                self.buffer = self.buffer[index + 2 :]
                event = self._parse(block)
                if event is not None:
                    return event
                continue
            try:
                self.buffer += self._chunk()
            except EOFError:
                fail("SSE stream ended before an event arrived")

    @staticmethod
    def _parse(block):
        if not block or block.startswith(":"):
            return None
        event = {"event": "message", "id": None, "data": ""}
        data = []
        for line in block.split("\n"):
            if line.startswith("id:"):
                try:
                    event["id"] = int(line[3:].strip())
                except ValueError:
                    pass
            elif line.startswith("event:"):
                event["event"] = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].strip())
        if not data:
            return None
        event["data"] = "\n".join(data)
        return event

    def close(self):
        try:
            self.file.close()
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass


class Smoke:
    def __init__(self, args):
        self.args = args
        self.work = new_work_root()
        self.config = os.path.join(self.work, "config")
        self.data = os.path.join(self.work, "data")
        self.logs = os.path.join(self.work, "logs")
        self.trust = os.path.join(self.config, "trust.d")
        self.bin = os.path.join(self.work, "bin")
        self.out = os.path.abspath(args.out)
        self.env = os.environ.copy()
        self.daemon = None
        self.daemon_log_path = os.path.join(self.work, "daemon.log")
        self.port = None
        self.http = None
        self.artifacts = {}

    # ---------------------------------------------------------------- setup
    def prepare(self):
        for directory in (self.config, self.data, self.logs, self.trust, self.bin, self.out):
            os.makedirs(directory, exist_ok=True)
        os.makedirs(os.path.join(self.config, "services.d"), exist_ok=True)

        binary = os.path.abspath(self.args.binary)
        require(os.path.isfile(binary), "svcd binary not found: " + binary)
        with open(binary, "rb") as handle:
            magic = handle.read(4)
        require(magic == b"\x7fELF", "not a Linux ELF binary: %s (build it inside WSL)" % binary)

        for name in ("svcd", "svc", "plugin", "mcp"):
            target = os.path.join(self.bin, name)
            if os.path.lexists(target):
                os.unlink(target)
            os.symlink(binary, target)

        key = os.path.abspath(self.args.key)
        pub = os.path.abspath(self.args.pub)
        require(os.path.isfile(key), "signing key not found: " + key + " (run node tools/gen_sample_key.mjs)")
        require(os.path.isfile(pub), "public key not found: " + pub)
        shutil.copyfile(pub, os.path.join(self.trust, self.args.key_id + ".pub"))

        with open(os.path.join(self.config, "svcd.conf"), "w", encoding="utf-8") as handle:
            handle.write("# generated by tools/spk_smoke.py\n")
            handle.write("http_port = 0\n")

        if self.args.lan:
            # The smoke daemon normally binds loopback only. `--lan` lets a
            # browser on the Windows side use the WSL address directly.
            with open(os.path.join(self.config, "lan.json"), "w", encoding="utf-8") as handle:
                json.dump({"enabled": True, "bind": "0.0.0.0"}, handle)

        daemon_env = self.env.copy()
        daemon_env.update(
            SVCD_CONFIG_DIR=self.config,
            SVCD_DATA_DIR=self.data,
            SVCD_LOG_DIR=self.logs,
            SVCD_TRUST_DIR=self.trust,
            SVCD_HTTP_PORT=str(self.args.port) if self.args.serve else "0",
            SVCD_TEST_UNPRIVILEGED="1",
        )
        self.env = daemon_env

        require(shutil.which("python3") is not None, "python3 is required for the plugin payloads")

        for name in PACKAGES:
            source = os.path.join(SOURCES, name)
            require(os.path.isdir(source), "missing package source: " + source)
            staged = os.path.join(self.out, name + ".spk")
            result = run(
                [
                    os.path.join(self.bin, "plugin"),
                    "bundle",
                    source,
                    "--key",
                    key,
                    "--key-id",
                    self.args.key_id,
                    "-o",
                    staged,
                ],
                self.env,
                cwd=self.out,
            )
            require(result.returncode == 0, "bundle %s failed:\n%s" % (name, result.stdout))
            # Name the artifact from the manifest the bundler actually wrote, so
            # a stale build with another version or target triple is never picked.
            with tarfile.open(staged) as archive:
                manifest = json.load(archive.extractfile("manifest.json"))
            arch = str(manifest["platform"]).split("-")[0]
            final = os.path.join(self.out, "%s-%s-%s.spk" % (manifest["name"], manifest["version"], arch))
            os.replace(staged, final)
            self.artifacts[name] = final
            print("  built %s" % final)

        log = open(self.daemon_log_path, "wb")
        self.daemon = subprocess.Popen(
            [os.path.join(self.bin, "svcd"), "run", "--foreground"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=log,
            env=self.env,
        )
        self._wait_for_http()
        # The password is written only through the documented writer, so the
        # harness never owns the PBKDF2 wire format. `svc auth passwd` needs
        # root, or a debug build with SVCD_TEST_UNPRIVILEGED=1 (which this env
        # already sets) — that is why CI runs the smoke against target/debug.
        code, output = cli(os.path.join(self.bin, "svc"), ["auth", "passwd", PASSWORD], self.env)
        require(
            code == 0,
            "svc auth passwd failed (%d): %s\nrun as root or use a debug build (SVCD_TEST_UNPRIVILEGED=1)"
            % (code, output),
        )

    def _wait_for_http(self):
        port_file = os.path.join(self.config, "http.port")
        deadline = time.time() + 20
        while time.time() < deadline:
            if self.daemon.poll() is not None:
                fail("daemon exited during startup:\n" + self.daemon_log())
            try:
                with open(port_file, "r", encoding="utf-8") as handle:
                    address = handle.read().strip()
                self.port = int(address.rsplit(":", 1)[1])
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                self.http = Http(self.port)
                return
            except (OSError, ValueError, IndexError):
                time.sleep(0.05)
        fail("daemon never opened its HTTP port:\n" + self.daemon_log())

    def daemon_log(self):
        try:
            with open(self.daemon_log_path, "r", encoding="utf-8", errors="replace") as handle:
                return handle.read()[-6000:]
        except OSError:
            return ""

    def install(self, name):
        code, output = cli(os.path.join(self.bin, "plugin"), ["install", self.artifacts[name]], self.env)
        require(code == 0, "plugin install %s failed (%d):\n%s" % (name, code, output))
        return output

    def start(self, name, should_fail=False):
        code, output = cli(os.path.join(self.bin, "svc"), ["start", name], self.env)
        if should_fail:
            require(code != 0, "svc start %s unexpectedly succeeded:\n%s" % (name, output))
            return output
        require(code == 0, "svc start %s failed (%d):\n%s" % (name, code, output))
        return output

    def status(self, name):
        code, output = cli(os.path.join(self.bin, "svc"), ["status", name, "--json"], self.env)
        require(code == 0, "svc status %s failed (%d):\n%s" % (name, code, output))
        return json.loads(output[output.index("{") :])

    def service_state(self, name):
        return self.status(name)["summary"]["state"]

    # -------------------------------------------------------------- sections
    def check_dependencies(self, report):
        def install_app_first():
            self.install("dep-app")
            return "dep-app installed before dep-base"

        def refuses_without_base():
            output = self.start("dep-app", should_fail=True)
            lowered = output.lower()
            require(
                "missing dependencies" in lowered and "dep-base" in lowered,
                "start error did not name dep-base: " + output,
            )
            return "refused: missing dependencies: dep-base"

        def base_arrives():
            self.install("dep-base")
            self.start("dep-app")
            require(self.service_state("dep-app") == "running", "dep-app is not running")
            return "dep-app running"

        def dependency_was_started():
            require(self.service_state("dep-base") == "running", "dep-base was not started as a dependency")
            app = self.status("dep-app")
            require(
                app["details"]["dependencies"] == ["dep-base"],
                "dep-app status dependencies = %r" % app["details"]["dependencies"],
            )
            return "dep-base running; dependencies=[dep-base]"

        def stop_app_keeps_base():
            code, output = cli(os.path.join(self.bin, "svc"), ["stop", "dep-app"], self.env)
            require(code == 0, "stop dep-app failed:\n" + output)
            require(self.service_state("dep-app") == "stopped", "dep-app did not stop")
            require(self.service_state("dep-base") == "running", "dep-base stopped with its dependent")
            return "stopping dep-app leaves dep-base running"

        report.check("install dep-app before dep-base", install_app_first)
        report.check("start refuses without dep-base", refuses_without_base)
        report.check("start pulls dep-base in first", base_arrives)
        report.check("status records the dependency edge", dependency_was_started)
        report.check("stopping a dependent leaves the base", stop_app_keeps_base)

    def check_mcp(self, report):
        def install_and_start():
            self.install("mcp-echo")
            self.start("mcp-echo")
            require(self.service_state("mcp-echo") == "running", "mcp-echo is not running")
            return "mcp-echo running"

        def lists_capabilities():
            status, body = mcp_http(self.http, "tools/list", {})
            require(status == 200, "tools/list status %d: %r" % (status, body))
            names = tool_names(body)
            for expected in ("mcp-echo.echo", "mcp-echo.add", "mcp-echo.status", "mcp-echo.start"):
                require(expected in names, "missing tool %s in %r" % (expected, names))
            return ", ".join(sorted(name for name in names if name.startswith("mcp-echo.")))

        def calls_echo():
            status, body = mcp_http(
                self.http,
                "tools/call",
                {"name": "mcp-echo.echo", "arguments": {"text": "hello-mcp"}},
                name="mcp-echo.echo",
            )
            require(status == 200, "tools/call status %d: %r" % (status, body))
            result = body.get("result", {})
            require(result.get("isError") is False, "echo was an error: %r" % body)
            text = tool_text(body)
            require(text == "mcp-echo: hello-mcp", "unexpected echo text: %r" % text)
            return text

        def calls_add():
            status, body = mcp_http(
                self.http,
                "tools/call",
                {"name": "mcp-echo.add", "arguments": {"a": 19, "b": 23}},
                name="mcp-echo.add",
            )
            require(status == 200, "add status %d: %r" % (status, body))
            text = tool_text(body)
            require(text == "42", "unexpected add result: %r" % text)
            return "19 + 23 = " + text

        def rejects_missing_bearer():
            status, _, body = self.http.json("POST", "/mcp", {"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
            require(status == 401, "missing bearer got status %d: %r" % (status, body))
            return "401 without Authorization"

        def rejects_wrong_bearer():
            status, body = mcp_http(self.http, "tools/list", {}, password="wrong-password")
            require(status == 403, "wrong bearer got status %d: %r" % (status, body))
            return "403 with a wrong token"

        def stdio_modern():
            bridge = StdioBridge(os.path.join(self.bin, "mcp"), self.env)
            try:
                listed = bridge.call("tools/list", {"_meta": modern_meta()})
                names = tool_names(listed)
                require("mcp-echo.echo" in names, "stdio tools/list missing mcp-echo.echo: %r" % names)
                called = bridge.call(
                    "tools/call",
                    {
                        "name": "mcp-echo.echo",
                        "arguments": {"text": "hello-stdio"},
                        "_meta": modern_meta(),
                    },
                )
                text = tool_text(called)
                require(text == "mcp-echo: hello-stdio", "stdio echo mismatch: %r" % text)
                return text
            finally:
                bridge.close()

        def stdio_legacy():
            bridge = StdioBridge(os.path.join(self.bin, "mcp"), self.env)
            try:
                initialized = bridge.call(
                    "initialize",
                    {
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "spk-smoke", "version": "1.0.0"},
                    },
                )
                require("result" in initialized, "legacy initialize failed: %r" % initialized)
                bridge.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
                listed = bridge.call("tools/list", {})
                names = tool_names(listed)
                require("mcp-echo.echo" in names, "legacy tools/list missing mcp-echo.echo: %r" % names)
                return "initialize 2025-11-25 -> %d tools" % len(names)
            finally:
                bridge.close()

        report.check("install and start mcp-echo", install_and_start)
        report.check("tools/list exposes mcp-echo.*", lists_capabilities)
        report.check("tools/call echo", calls_echo)
        report.check("tools/call add", calls_add)
        report.check("MCP rejects a missing Bearer", rejects_missing_bearer)
        report.check("MCP rejects a wrong Bearer", rejects_wrong_bearer)
        report.check("stdio bridge (modern)", stdio_modern)
        report.check("stdio bridge (legacy initialize)", stdio_legacy)

    def check_websocket(self, report):
        def install_and_start():
            self.install("ws-panel")
            self.start("ws-panel")
            require(self.service_state("ws-panel") == "running", "ws-panel is not running")
            return "ws-panel running"

        def ui_page():
            status, headers, text = self.http.text("/plugin-ui/ws-panel/")
            require(status == 200, "ws-panel UI status %d" % status)
            require("new WebSocket(" in text, "UI page does not open a WebSocket")
            require('/plugin-ui/" + pluginId + "/ws' in text or "/plugin-ui/ws-panel/ws" in text, "UI page does not name the /ws endpoint")
            require("Emit notification" in text, "UI page is missing its controls")
            return "GET /plugin-ui/ws-panel/ 200 (%d B)" % len(text.encode("utf-8"))

        def upgrade_is_required():
            status, headers, text = self.http.text("/plugin-ui/ws-panel/ws")
            require(status == 426, "plain GET /ws got %d, expected 426" % status)
            return "426 without an Upgrade header"

        def handshake_and_call():
            client = WsClient(self.port, "/plugin-ui/ws-panel/ws")
            try:
                client.send_frame(
                    0x1,
                    json.dumps(
                        {
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "tools/call",
                            "params": {"name": "status", "arguments": {}},
                        }
                    ),
                )
                deadline = time.time() + 8
                seen_tick = False
                response = None
                while time.time() < deadline and (not seen_tick or response is None):
                    message = client.next_message()
                    require("__close__" not in message, "WebSocket closed early: %r" % message)
                    if message.get("method") == "notifications/message":
                        data = message.get("params", {}).get("data")
                        if isinstance(data, dict) and data.get("kind") == "tick":
                            seen_tick = True
                    elif message.get("id") == 1:
                        response = message
                require(seen_tick, "no notifications/message tick arrived over WebSocket")
                require(response is not None, "no response to tools/call status")
                result = response.get("result", {})
                require(result.get("isError") is False, "status was an error: %r" % response)
                payload = json.loads(tool_text(response))
                require(payload.get("service") == "ws-panel", "unexpected status payload: %r" % payload)
                return "notification + status response on one socket"
            finally:
                client.close()

        def emit_and_pong():
            client = WsClient(self.port, "/plugin-ui/ws-panel/ws")
            try:
                client.send_frame(
                    0x1,
                    json.dumps(
                        {
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "tools/call",
                            "params": {"name": "emit", "arguments": {"text": "hello-ws"}},
                        }
                    ),
                )
                deadline = time.time() + 8
                notified = False
                replied = False
                while time.time() < deadline and not (notified and replied):
                    message = client.next_message()
                    if message.get("method") == "notifications/message":
                        data = message.get("params", {}).get("data")
                        if isinstance(data, dict) and data.get("text") == "hello-ws":
                            notified = True
                    elif message.get("id") == 2:
                        replied = True
                require(notified, "emit did not produce a notification")
                require(replied, "emit did not reply to the caller")

                client.send_frame(0x9, b"ping")
                deadline = time.time() + 4
                pong = False
                while time.time() < deadline:
                    opcode, payload = client.read_frame()
                    if opcode == 0xA:
                        pong = payload == b"ping"
                        break
                require(pong, "no pong for a ping frame")
                return "emit round-trip + ping/pong"
            finally:
                client.close()

        def close_frame():
            client = WsClient(self.port, "/plugin-ui/ws-panel/ws")
            try:
                client.send_frame(0x8, struct.pack("!H", 1000))
                deadline = time.time() + 5
                while time.time() < deadline:
                    opcode, payload = client.read_frame()
                    if opcode == 0x8:
                        code = struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 0
                        require(code == 1000, "close reply code was %d" % code)
                        return "close 1000 acknowledged"
            finally:
                client.close()
            fail("no close frame reply")

        report.check("install and start ws-panel", install_and_start)
        report.check("ws-panel web UI is served", ui_page)
        report.check("WebSocket upgrade is required", upgrade_is_required)
        report.check("WS handshake + notification + call", handshake_and_call)
        report.check("WS emit round-trip + ping/pong", emit_and_pong)
        report.check("WS close frame", close_frame)

    def check_sse(self, report):
        def install_and_start():
            self.install("sse-panel")
            self.start("sse-panel")
            require(self.service_state("sse-panel") == "running", "sse-panel is not running")
            return "sse-panel running"

        def ui_page():
            status, headers, text = self.http.text("/plugin-ui/sse-panel/")
            require(status == 200, "sse-panel UI status %d" % status)
            require("new EventSource(" in text, "UI page does not open an EventSource")
            require("Last-Event-ID" in text, "UI page does not demonstrate Last-Event-ID")
            require("Emit notification" in text, "UI page is missing its controls")
            return "GET /plugin-ui/sse-panel/ 200 (%d B)" % len(text.encode("utf-8"))

        def stream_and_rpc():
            stream = SseClient(self.port, "/plugin-ui/sse-panel/events")
            try:
                event = stream.next_event()
                require(event["id"] is not None, "SSE event has no id: %r" % event)
                require(event["event"] == "message", "SSE event name is %r" % event["event"])
                payload = json.loads(event["data"])
                require(payload.get("method") == "notifications/message", "unexpected SSE payload: %r" % payload)

                status, _, body = self.http.json(
                    "POST",
                    "/plugin-ui/sse-panel/rpc",
                    {
                        "jsonrpc": "2.0",
                        "id": 7,
                        "method": "tools/call",
                        "params": {"name": "emit", "arguments": {"text": "hello-sse"}},
                    },
                )
                require(status == 200, "POST /rpc status %d: %r" % (status, body))
                require(body["result"]["isError"] is False, "emit via /rpc failed: %r" % body)

                deadline = time.time() + 8
                while time.time() < deadline:
                    event = stream.next_event()
                    payload = json.loads(event["data"])
                    data = payload.get("params", {}).get("data")
                    if isinstance(data, dict) and data.get("text") == "hello-sse":
                        return "event id=%s + emit via POST /rpc" % event["id"]
                fail("emit notification never arrived on the SSE stream")
            finally:
                stream.close()

        def replay_after_reconnect():
            stream = SseClient(self.port, "/plugin-ui/sse-panel/events")
            try:
                event = stream.next_event()
                checkpoint = event["id"]
            finally:
                stream.close()

            status, _, body = self.http.json(
                "POST",
                "/plugin-ui/sse-panel/rpc",
                {
                    "jsonrpc": "2.0",
                    "id": 8,
                    "method": "tools/call",
                    "params": {"name": "emit", "arguments": {"text": "replay-one"}},
                },
            )
            require(status == 200 and body["result"]["isError"] is False, "emit replay-one failed: %r" % body)
            status, _, body = self.http.json(
                "POST",
                "/plugin-ui/sse-panel/rpc",
                {
                    "jsonrpc": "2.0",
                    "id": 9,
                    "method": "tools/call",
                    "params": {"name": "emit", "arguments": {"text": "replay-two"}},
                },
            )
            require(status == 200 and body["result"]["isError"] is False, "emit replay-two failed: %r" % body)

            stream = SseClient(self.port, "/plugin-ui/sse-panel/events", last_event_id=checkpoint)
            try:
                seen = []
                deadline = time.time() + 10
                while time.time() < deadline and len(seen) < 2:
                    event = stream.next_event()
                    require(event["id"] > checkpoint, "replayed event id %s is not > %s" % (event["id"], checkpoint))
                    payload = json.loads(event["data"])
                    data = payload.get("params", {}).get("data")
                    if isinstance(data, dict) and data.get("text") in ("replay-one", "replay-two"):
                        seen.append(data["text"])
                require(
                    sorted(seen) == ["replay-one", "replay-two"],
                    "replay did not recover both events: %r" % seen,
                )
                return "Last-Event-ID %s replayed %s" % (checkpoint, ", ".join(sorted(seen)))
            finally:
                stream.close()

        report.check("install and start sse-panel", install_and_start)
        report.check("sse-panel web UI is served", ui_page)
        report.check("SSE stream + POST /rpc uplink", stream_and_rpc)
        report.check("SSE Last-Event-ID replay", replay_after_reconnect)

    # ---------------------------------------------------------------- teardown
    def shutdown(self):
        if self.daemon is None:
            return
        try:
            run([os.path.join(self.bin, "svcd"), "shutdown"], self.env, timeout=15)
        except Exception:
            pass
        try:
            self.daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait()

    def cleanup(self):
        self.shutdown()
        if not self.args.keep:
            shutil.rmtree(self.work, ignore_errors=True)


def parse_args(argv):
    parser = argparse.ArgumentParser(description="Smoke-test the WSL-installable sample .spk packages.")
    parser.add_argument("--binary", default=os.path.join(ROOT, "target", "release", "svcd"), help="Linux svcd binary")
    parser.add_argument("--key", default=os.path.join(ROOT, "samples", "keys", "sample-2026.key"))
    parser.add_argument("--pub", default=os.path.join(ROOT, "samples", "keys", "sample-2026.pub"))
    parser.add_argument("--key-id", default="sample-2026")
    parser.add_argument("--out", default=os.path.join(ROOT, "samples", "spk"), help="where built .spk files go")
    parser.add_argument("--keep", action="store_true", help="keep the temp config/data tree")
    parser.add_argument("--report", default=None, help="Markdown report path (default target/spk-smoke.md)")
    parser.add_argument("--serve", action="store_true", help="install, start and keep the demo running for the web UI")
    parser.add_argument("--port", type=int, default=18787, help="fixed HTTP port for --serve")
    parser.add_argument("--lan", action="store_true", help="with --serve, bind 0.0.0.0 instead of loopback")
    return parser.parse_args(argv)


def run_checks(smoke, report):
    print("\n[1/4] package dependencies")
    smoke.check_dependencies(report)
    print("\n[2/4] MCP")
    smoke.check_mcp(report)
    print("\n[3/4] WebSocket")
    smoke.check_websocket(report)
    print("\n[4/4] SSE")
    smoke.check_sse(report)


def serve(smoke, args):
    """Install and start every sample, then block so a browser can watch."""
    for name in PACKAGES:
        smoke.install(name)
    smoke.start("dep-app")  # pulls dep-base in first
    smoke.start("mcp-echo")
    smoke.start("ws-panel")
    smoke.start("sse-panel")

    host = "127.0.0.1" if not args.lan else wsl_address()
    print("\nserving the samples on port %d — Ctrl+C to stop" % smoke.port)
    print("  ws-panel  http://%s:%d/plugin-ui/ws-panel/" % (host, smoke.port))
    print("  sse-panel http://%s:%d/plugin-ui/sse-panel/" % (host, smoke.port))
    print("  svcd UI   http://%s:%d/" % (host, smoke.port))
    if args.lan:
        print("  LAN auth is on; log in with password: %s" % PASSWORD)
    print("\nDSH MCP link (transport: streamable-http):")
    print("  url: http://%s:%d/mcp" % (host, smoke.port))
    print("  header: Authorization: Bearer %s" % PASSWORD)
    print("\nwork tree: %s" % smoke.work)
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        print("\nstopping")
    return 0


def wsl_address():
    try:
        output = subprocess.run(
            ["hostname", "-I"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=5
        ).stdout.split()
        if output:
            return output[0]
    except (OSError, subprocess.SubprocessError):
        pass
    return "127.0.0.1"


def main(argv):
    args = parse_args(argv)
    if args.lan and not args.serve:
        print("--lan only makes sense with --serve")
        return 2
    print("spk smoke · %s" % ("WSL" if is_wsl() else sys.platform))
    smoke = Smoke(args)
    report = Report("spk smoke test report")
    started = time.time()
    try:
        smoke.prepare()
        if args.serve:
            return serve(smoke, args)

        run_checks(smoke, report)

        elapsed = time.time() - started
        print("\n%d/%d checks passed in %.1fs" % (sum(1 for row in report.rows if row["ok"]), len(report.rows), elapsed))
        print("\nThe smoke run shuts the daemon down on exit. Use --serve to keep the samples")
        print("running and open the web UIs at:")
        print("  http://127.0.0.1:%d/plugin-ui/ws-panel/" % smoke.port)
        print("  http://127.0.0.1:%d/plugin-ui/sse-panel/" % smoke.port)
        print("\nDSH MCP link (transport: streamable-http):")
        print("  url: http://127.0.0.1:%d/mcp" % smoke.port)
        print("  header: Authorization: Bearer %s" % PASSWORD)

        extra = {
            "Environment": "%s, python %s" % ("WSL" if is_wsl() else sys.platform, sys.version.split()[0]),
            "svcd": args.binary,
            "Work tree": smoke.work,
            "Artifacts": ", ".join(sorted(os.path.basename(path) for path in smoke.artifacts.values())),
            "HTTP port": smoke.port,
        }
        target = os.path.join(ROOT, "target")
        os.makedirs(target, exist_ok=True)
        markdown_path = args.report or os.path.join(target, "spk-smoke.md")
        with open(markdown_path, "w", encoding="utf-8") as handle:
            handle.write(report.to_markdown(extra))
        with open(os.path.join(target, "spk-smoke.json"), "w", encoding="utf-8") as handle:
            json.dump(
                {
                    "environment": extra,
                    "checks": report.rows,
                    "artifacts": smoke.artifacts,
                    "web_ui": {
                        "ws": "http://127.0.0.1:%d/plugin-ui/ws-panel/" % smoke.port,
                        "sse": "http://127.0.0.1:%d/plugin-ui/sse-panel/" % smoke.port,
                    },
                    "dsh_mcp": {
                        "transport": "streamable-http",
                        "url": "http://127.0.0.1:%d/mcp" % smoke.port,
                        "authorization": "Bearer " + PASSWORD,
                    },
                },
                handle,
                indent=2,
            )
        print("\nreport: %s" % markdown_path)
        return 0 if report.passed() else 1
    except SmokeError as error:
        print("\nSMOKE FAILED: %s" % error)
        print("\ndaemon log tail:\n%s" % smoke.daemon_log())
        return 1
    finally:
        smoke.cleanup()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
