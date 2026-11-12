use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
    config: PathBuf,
    data: PathBuf,
    logs: PathBuf,
    svcd: PathBuf,
    svc: PathBuf,
    mcp: PathBuf,
    daemon: Option<Child>,
    daemon_stderr: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "svcd-mcp-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let config = root.join("config");
        let data = root.join("data");
        let logs = root.join("logs");
        fs::create_dir_all(&config).expect("config");
        fs::create_dir_all(&data).expect("data");
        fs::create_dir_all(&logs).expect("logs");

        let binary = env!("CARGO_BIN_EXE_svcd");
        let names = if cfg!(windows) {
            ("svcd.exe", "svc.exe", "mcp.exe")
        } else {
            ("svcd", "svc", "mcp")
        };
        let svcd = root.join(names.0);
        let svc = root.join(names.1);
        let mcp = root.join(names.2);
        for path in [&svcd, &svc, &mcp] {
            fs::copy(binary, path).expect("copy multicall");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
            }
        }

        let sandbox = Sandbox {
            daemon_stderr: root.join("daemon.stderr"),
            root,
            config,
            data,
            logs,
            svcd,
            svc,
            mcp,
            daemon: None,
        };
        sandbox.write_plugin();
        sandbox
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_HTTP_PORT", "0");
    }

    fn write_plugin(&self) {
        let plugin = self.data.join("plugins").join("demo");
        let version = plugin.join("1.0.0");
        let bin = version.join("payload").join("bin");
        fs::create_dir_all(&bin).expect("plugin bin");
        fs::write(plugin.join("current"), "1.0.0").expect("current");

        let exec = {
            #[cfg(windows)]
            {
                let cmd = bin.join("agent.cmd");
                let ps1 = bin.join("agent.ps1");
                fs::write(
                    &cmd,
                    "@echo off\r\npowershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0agent.ps1\"\r\n",
                )
                .expect("cmd");
                fs::write(
                    &ps1,
                    r#"$reader = [Console]::In
while ($null -ne ($line = $reader.ReadLine())) {
  $msg = $line | ConvertFrom-Json
  if ($msg.method -eq 'svcd.ping') {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"ok":true}}')
    [Console]::Out.Flush()
    continue
  }
  if ($msg.method -eq 'tools/call') {
    $name = $msg.params.name
    if ($name -eq 'echo') {
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}'
    } elseif ($name -eq 'fail') {
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"error":{"code":-32000,"message":"boom"}}'
    } else {
      continue
    }
    [Console]::Out.WriteLine($out)
    [Console]::Out.Flush()
  }
}
"#,
                )
                .expect("ps1");
                "payload/bin/agent.cmd"
            }
            #[cfg(unix)]
            {
                let script = bin.join("agent.sh");
                fs::write(
                    &script,
                    r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n1 | cut -d: -f2)
  case "$line" in
    *svcd.ping*) printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id" ;;
    *'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id" ;;
    *'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"boom"}}\n' "$id" ;;
  esac
done
"#,
                )
                .expect("script");
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
                    .expect("chmod script");
                "payload/bin/agent.sh"
            }
        };

        let manifest = json!({
            "schema": 1,
            "name": "demo",
            "version": "1.0.0",
            "desc": "mcp bridge test plugin",
            "platform": target(),
            "protocol": 1,
            "exec": {"path": exec, "args": [], "ready": "notify"},
            "capabilities": [
                {"name": "echo", "desc": "Echo", "inputSchema": {"type": "object"}},
                {"name": "fail", "desc": "Fail", "inputSchema": {"type": "object"}}
            ],
            "files": []
        });
        fs::write(
            version.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("write manifest");
    }

    fn start_daemon(&mut self) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("daemon stderr");
        let mut command = Command::new(&self.svcd);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        self.env(&mut command);
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    fn wait_for_daemon(&self) {
        for _ in 0..300 {
            let output = self.run_svc(&["list"]);
            if output.status.code() != Some(2) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon never became reachable: {}", self.daemon_log());
    }

    fn run_svc(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(&self.svc);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svc")
    }

    fn start_plugin(&self) {
        let output = self.run_svc(&["start", "demo"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "svc start demo failed: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            self.daemon_log()
        );
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(&self.daemon_stderr).unwrap_or_default()
    }

    fn spawn_mcp(&self, tag: &str) -> McpClient {
        let mut command = Command::new(&self.mcp);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(
                fs::File::create(self.root.join(format!("mcp-{}.stderr", tag)))
                    .expect("mcp stderr"),
            ));
        self.env(&mut command);
        McpClient::new(
            command.spawn().expect("spawn mcp"),
            self.root.join(format!("mcp-{}.stderr", tag)),
        )
    }

    fn shutdown(&mut self) {
        if self.daemon.is_some() {
            let mut command = Command::new(&self.svcd);
            command.arg("shutdown");
            self.env(&mut command);
            let _ = command.output();
            if let Some(mut child) = self.daemon.take() {
                for _ in 0..200 {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn target() -> &'static str {
    if cfg!(all(windows, target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "aarch64",
        target_env = "musl"
    )) {
        "aarch64-unknown-linux-musl"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "arm",
        target_env = "musl"
    )) {
        "armv7-unknown-linux-musleabihf"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    stderr_path: PathBuf,
}

impl McpClient {
    fn new(mut child: Child, stderr_path: PathBuf) -> Self {
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = child.stdout.take().expect("mcp stdout");
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("mcp-test-reader".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            if sender.send(line).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("reader thread");
        McpClient {
            child,
            stdin,
            lines: receiver,
            stderr_path,
        }
    }

    fn send(&mut self, value: Value) {
        writeln!(
            self.stdin,
            "{}",
            serde_json::to_string(&value).expect("serialize request")
        )
        .expect("write mcp request");
        self.stdin.flush().expect("flush mcp request");
    }

    fn recv(&self, timeout: Duration) -> Value {
        match self.lines.recv_timeout(timeout) {
            Ok(line) => serde_json::from_str(&line).unwrap_or_else(|error| {
                panic!(
                    "invalid MCP line {line:?}: {error}; stderr: {}",
                    self.stderr()
                )
            }),
            Err(error) => panic!("no MCP line ({error}); stderr: {}", self.stderr()),
        }
    }

    fn recv_response(&self, id: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let value = self.recv(remaining);
            if value.get("method").is_some() && value.get("id").is_none() {
                continue;
            }
            assert_eq!(value.get("id"), Some(&json!(id)), "response id: {value}");
            return value;
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.recv_response(id, Duration::from_secs(10))
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("mcp did not exit; stderr: {}", self.stderr());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn modern_meta() -> Value {
    json!({
        "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientCapabilities": {}
        }
    })
}

fn modern_params(params: Value) -> Value {
    let mut params = params;
    if !params.is_object() {
        params = json!({});
    }
    params["_meta"] = modern_meta()["_meta"].clone();
    params
}

fn tool_names(tools: &Value) -> Vec<&str> {
    tools
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn modern_bridge_discovers_lists_and_calls_tools() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.start_plugin();
    let mut mcp = sandbox.spawn_mcp("modern");

    let discover = mcp.request(1, "server/discover", modern_params(json!({})));
    let result = discover.get("result").expect("discover result");
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(result["ttlMs"], 1000);
    assert_eq!(result["cacheScope"], "private");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "svcd"
    );

    let listed = mcp.request(2, "tools/list", modern_params(json!({})));
    let result = listed.get("result").expect("list result");
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 1000);
    assert_eq!(result["cacheScope"], "private");
    let names = tool_names(&result["tools"]);
    assert!(names.contains(&"demo.echo"), "{names:?}");
    assert!(names.contains(&"demo.fail"), "{names:?}");
    assert!(names.contains(&"demo.start"), "{names:?}");
    let echo = result["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["name"] == "demo.echo")
        .expect("echo tool");
    assert_eq!(echo["inputSchema"]["type"], "object");

    let called = mcp.request(
        3,
        "tools/call",
        modern_params(json!({"name": "demo.echo", "arguments": {}})),
    );
    let result = called.get("result").expect("call result");
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["text"], "pong");

    let failed = mcp.request(
        4,
        "tools/call",
        modern_params(json!({"name": "demo.fail", "arguments": {}})),
    );
    let result = failed.get("result").expect("failure result");
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "boom");

    let unknown = mcp.request(
        5,
        "tools/call",
        modern_params(json!({"name": "demo.nope", "arguments": {}})),
    );
    assert_eq!(unknown["error"]["code"], -32602);

    let missing = mcp.request(6, "tools/list", json!({}));
    assert_eq!(missing["error"]["code"], -32602);

    let bad_version = mcp.request(
        7,
        "tools/list",
        json!({"_meta": {
            "io.modelcontextprotocol/protocolVersion": "1900-01-01",
            "io.modelcontextprotocol/clientCapabilities": {}
        }}),
    );
    assert_eq!(bad_version["error"]["code"], -32022);
    assert_eq!(
        bad_version["error"]["data"]["supported"],
        json!(["2026-07-28"])
    );
}

#[test]
fn legacy_bridge_initializes_and_notifies_list_changes() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let mut mcp = sandbox.spawn_mcp("legacy");

    let initialized = mcp.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}
        }),
    );
    let result = initialized.get("result").expect("initialize result");
    assert_eq!(result["protocolVersion"], "2025-11-25");
    assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
    assert_eq!(result["serverInfo"]["name"], "svcd");
    assert!(result.get("resultType").is_none());

    mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let ping = mcp.request(2, "ping", json!({}));
    assert_eq!(ping["result"], json!({}));

    let listed = mcp.request(3, "tools/list", json!({}));
    let result = listed.get("result").expect("legacy list result");
    assert!(result.get("resultType").is_none());
    assert!(result.get("ttlMs").is_none());
    let names = tool_names(&result["tools"]);
    assert_eq!(names, vec!["demo.start"]);

    // Let the bridge take its first baseline poll before the list changes.
    std::thread::sleep(Duration::from_millis(1500));
    sandbox.start_plugin();

    // Keep the client busy while the list changes: a client that sends more
    // than one request per second must not starve the 1s list_changed poll.
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut id = 10;
    let mut saw_list_changed = false;
    while Instant::now() < deadline && !saw_list_changed {
        mcp.send(json!({"jsonrpc": "2.0", "id": id, "method": "ping", "params": {}}));
        loop {
            let value = mcp.recv(Duration::from_secs(2));
            if value.get("method") == Some(&json!("notifications/tools/list_changed")) {
                saw_list_changed = true;
                continue;
            }
            if value.get("id") == Some(&json!(id)) {
                break;
            }
        }
        id += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(saw_list_changed, "busy legacy client starved list_changed");

    let listed = mcp.request(4, "tools/list", json!({}));
    let result = listed.get("result").expect("legacy list result");
    let names = tool_names(&result["tools"]);
    assert!(names.contains(&"demo.echo"), "{names:?}");

    let called = mcp.request(
        5,
        "tools/call",
        json!({"name": "demo.echo", "arguments": {}}),
    );
    let result = called.get("result").expect("legacy call result");
    assert!(result.get("resultType").is_none());
    assert_eq!(result["content"][0]["text"], "pong");
}

#[test]
fn bridge_exits_one_when_svcd_is_unreachable() {
    let sandbox = Sandbox::new();
    let mut mcp = sandbox.spawn_mcp("offline");

    let status = mcp.wait_exit(Duration::from_secs(10));

    assert_eq!(status.code(), Some(1), "stderr: {}", mcp.stderr());
    assert!(
        mcp.stderr().contains("svcd unreachable"),
        "stderr: {}",
        mcp.stderr()
    );
}

#[test]
fn multiple_bridges_can_share_one_svcd() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.start_plugin();
    let mut one = sandbox.spawn_mcp("one");
    let mut two = sandbox.spawn_mcp("two");

    one.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": modern_params(json!({}))
    }));
    two.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": modern_params(json!({}))
    }));
    let first = one.recv_response(1, Duration::from_secs(10));
    let second = two.recv_response(1, Duration::from_secs(10));

    assert_eq!(
        tool_names(&first["result"]["tools"]),
        tool_names(&second["result"]["tools"])
    );
    assert!(tool_names(&first["result"]["tools"]).contains(&"demo.echo"));
}
