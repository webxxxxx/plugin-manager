use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
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
            .env("SVCD_HTTP_PORT", "0")
            .env("SVCD_TEST_UNPRIVILEGED", "1");
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
    } elseif ($name -eq 'progress') {
      $token = $msg.params._meta.progressToken
      if ($null -ne $token) {
        [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"' + $token + '"}}')
        [Console]::Out.Flush()
      }
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"content":[{"type":"text","text":"done"}],"isError":false}}'
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
    *'"name":"progress"'*)
      token=$(printf '%s' "$line" | grep -o '"progressToken":"[^"]*"' | head -n1 | cut -d'"' -f4)
      if [ -n "$token" ]; then
        printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"%s"}}\n' "$token"
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"done"}],"isError":false}}\n' "$id"
      ;;
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
                {"name": "fail", "desc": "Fail", "inputSchema": {"type": "object"}},
                {"name": "progress", "desc": "Progress", "inputSchema": {"type": "object"}}
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
        // `svc list` falls back to local definitions and exits 0 without a
        // daemon, so it cannot detect readiness. The control channel binds
        // before the HTTP API and the API writes `http.port`; wait for that.
        let _ = self.wait_for_http();
    }

    fn wait_for_http(&self) -> String {
        for _ in 0..200 {
            if let Ok(address) = fs::read_to_string(self.config.join("http.port")) {
                let address = address.trim().to_string();
                if std::net::TcpStream::connect(&address).is_ok() {
                    return address;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("HTTP API never became ready: {}", self.daemon_log());
    }

    /// Set the auth password without rebinding the listener, so /mcp can be
    /// exercised on the loopback surface.
    fn set_password(&self) -> String {
        let output = self.run_svc(&["auth", "passwd", TEST_PASSWORD]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "svc auth passwd failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        TEST_PASSWORD.to_string()
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

    fn stop_plugin(&self) {
        let output = self.run_svc(&["stop", "demo"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "svc stop demo failed: {}\n{}",
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

const TEST_PASSWORD: &str = "test-password";

/// Start a daemon with the HTTP surface ready and a password set without
/// rebinding the listener. `plugin` starts the demo fixture too.
fn http_sandbox(plugin: bool) -> (Sandbox, String, String) {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let address = sandbox.wait_for_http();
    if plugin {
        sandbox.start_plugin();
    }
    let token = sandbox.set_password();
    (sandbox, address, token)
}

fn assert_header_mismatch(response: &HttpResponse) {
    assert_eq!(response.status, 400, "{:?}", response.body);
    assert_eq!(response.json()["error"]["code"], -32020);
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

impl HttpResponse {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "invalid HTTP JSON body {:?}: {}",
                String::from_utf8_lossy(&self.body),
                error
            )
        })
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

fn http(
    address: &str,
    method: &str,
    path: &str,
    body: &[u8],
    headers: &[(String, String)],
) -> HttpResponse {
    let mut stream = std::net::TcpStream::connect(address).expect("connect http");
    let mut request = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        method,
        path,
        address,
        body.len()
    );
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write http headers");
    stream.write_all(body).expect("write http body");
    read_http_response(&mut stream)
}

/// Announce a larger body than is sent: the server must reject on
/// Content-Length before waiting for the bytes.
fn http_declared(
    address: &str,
    path: &str,
    declared: usize,
    headers: &[(String, String)],
) -> HttpResponse {
    let mut stream = std::net::TcpStream::connect(address).expect("connect http");
    let mut request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        path, address, declared
    );
    for (name, value) in headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("write http headers");
    read_http_response(&mut stream)
}

fn read_http_response(stream: &mut std::net::TcpStream) -> HttpResponse {
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read http response");
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response headers");
    let head = String::from_utf8_lossy(&response[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("HTTP status");
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    HttpResponse {
        status,
        body: response[split + 4..].to_vec(),
        headers,
    }
}

fn response_cookie(response: &HttpResponse, name: &str) -> Option<String> {
    response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("set-cookie"))
        .find_map(|(_, value)| {
            let (pair, _) = value.split_once(';')?;
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_string())
        })
}

fn modern_request(id: u64, method: &str, params: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": modern_params(params),
    }))
    .expect("serialize modern request")
}

fn modern_request_versioned(id: u64, method: &str, params: Value, version: &str) -> Vec<u8> {
    let mut params = modern_params(params);
    params["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!(version);
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }))
    .expect("serialize versioned request")
}

fn mcp_headers(method: &str, version: &str, name: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("MCP-Protocol-Version".to_string(), version.to_string()),
        ("Mcp-Method".to_string(), method.to_string()),
    ];
    if let Some(name) = name {
        headers.push(("Mcp-Name".to_string(), name.to_string()));
    }
    headers
}

fn authorized(mut headers: Vec<(String, String)>, token: &str) -> Vec<(String, String)> {
    headers.push(("Authorization".to_string(), format!("Bearer {}", token)));
    headers
}

fn mcp_post(
    address: &str,
    token: &str,
    method: &str,
    body: &[u8],
    name: Option<&str>,
) -> HttpResponse {
    http(
        address,
        "POST",
        "/mcp",
        body,
        &authorized(mcp_headers(method, "2026-07-28", name), token),
    )
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
fn tools_call_forwards_plugin_progress() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.start_plugin();
    let mut mcp = sandbox.spawn_mcp("progress");

    mcp.request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}
        }),
    );
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "demo.progress",
            "arguments": {},
            "_meta": {"progressToken": "t1"}
        }
    }));

    let mut saw_progress = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = mcp.recv(deadline.saturating_duration_since(Instant::now()));
        if value.get("method") == Some(&json!("notifications/progress")) {
            assert_eq!(value["params"]["progressToken"], "t1");
            assert_eq!(value["params"]["progress"], 1);
            saw_progress = true;
            continue;
        }
        assert_eq!(value.get("id"), Some(&json!(2)), "{value}");
        assert_eq!(value["result"]["content"][0]["text"], "done");
        break;
    }
    assert!(saw_progress, "plugin progress was not forwarded");
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

#[test]
fn modern_bridge_paginates_tools_with_a_stable_cursor() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.start_plugin();
    let mut mcp = sandbox.spawn_mcp("paged");

    let first = mcp.request(1, "tools/list", modern_params(json!({"limit": 1})));
    let first_result = first.get("result").expect("first result");
    let first_names = tool_names(&first_result["tools"]);
    assert_eq!(first_names.len(), 1);
    let cursor = first_result["nextCursor"]
        .as_str()
        .expect("next cursor")
        .to_string();

    let second = mcp.request(
        2,
        "tools/list",
        modern_params(json!({"limit": 1, "cursor": cursor})),
    );
    let second_result = second.get("result").expect("second result");
    let second_names = tool_names(&second_result["tools"]);
    assert_eq!(second_names.len(), 1);
    assert_ne!(first_names, second_names);

    let bad = mcp.request(
        3,
        "tools/list",
        modern_params(json!({"limit": 1, "cursor": "bm90LWEtY3Vyc29y"})),
    );
    assert_eq!(bad["error"]["code"], -32602);
}

#[test]
fn http_mcp_requires_a_bearer_and_ignores_cookies() {
    let (_sandbox, address, token) = http_sandbox(true);

    let body = modern_request(1, "server/discover", json!({}));
    let headers = mcp_headers("server/discover", "2026-07-28", None);

    let missing = http(&address, "POST", "/mcp", &body, &headers);
    assert_eq!(missing.status, 401, "{:?}", missing.body);
    assert_eq!(missing.header("www-authenticate"), Some("Bearer"));

    let wrong = http(
        &address,
        "POST",
        "/mcp",
        &body,
        &authorized(headers.clone(), "not-the-token"),
    );
    assert_eq!(wrong.status, 403, "{:?}", wrong.body);

    // A valid management session cookie must not authenticate /mcp: the MCP
    // surface takes Bearer only, so it has no cookie/CSRF surface.
    let login_body = serde_json::to_vec(&json!({ "password": token })).expect("login body");
    let login = http(
        &address,
        "POST",
        "/login",
        &login_body,
        &[("Content-Type".to_string(), "application/json".to_string())],
    );
    assert_eq!(login.status, 200, "{:?}", login.body);
    let session_cookie = response_cookie(&login, "svcd_session").expect("session cookie");
    let mut cookie_headers = headers.clone();
    cookie_headers.push((
        "Cookie".to_string(),
        format!("svcd_session={}", session_cookie),
    ));
    let cookie_only = http(&address, "POST", "/mcp", &body, &cookie_headers);
    assert_eq!(cookie_only.status, 401, "{:?}", cookie_only.body);

    let get = http(&address, "GET", "/mcp", b"", &[]);
    assert_eq!(get.status, 405);
    assert_eq!(get.header("allow"), Some("POST"));
}

#[test]
fn http_mcp_discovers_lists_and_calls_with_stdio_semantics() {
    let (sandbox, address, token) = http_sandbox(true);

    let discover = mcp_post(
        &address,
        &token,
        "server/discover",
        &modern_request(1, "server/discover", json!({})),
        None,
    );
    assert_eq!(discover.status, 200, "{:?}", discover.body);
    assert_eq!(discover.header("content-type"), Some("application/json"));
    assert!(discover.header("mcp-session-id").is_none());
    assert!(!discover
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("set-cookie")));
    let result = discover.json()["result"].clone();
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(result["ttlMs"], 1000);
    assert_eq!(result["cacheScope"], "private");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "svcd"
    );

    let listed = mcp_post(
        &address,
        &token,
        "tools/list",
        &modern_request(2, "tools/list", json!({})),
        None,
    );
    assert_eq!(listed.status, 200, "{:?}", listed.body);
    let result = listed.json()["result"].clone();
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["ttlMs"], 1000);
    assert_eq!(result["cacheScope"], "private");
    let names = tool_names(&result["tools"]);
    assert!(names.contains(&"demo.echo"), "{names:?}");
    assert!(names.contains(&"demo.fail"), "{names:?}");
    assert!(names.contains(&"demo.start"), "{names:?}");

    let called = mcp_post(
        &address,
        &token,
        "tools/call",
        &modern_request(
            3,
            "tools/call",
            json!({"name": "demo.echo", "arguments": {}}),
        ),
        Some("demo.echo"),
    );
    assert_eq!(called.status, 200, "{:?}", called.body);
    let result = called.json()["result"].clone();
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["text"], "pong");

    let failed = mcp_post(
        &address,
        &token,
        "tools/call",
        &modern_request(
            4,
            "tools/call",
            json!({"name": "demo.fail", "arguments": {}}),
        ),
        Some("demo.fail"),
    );
    let result = failed.json()["result"].clone();
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "boom");

    let unknown_tool = mcp_post(
        &address,
        &token,
        "tools/call",
        &modern_request(
            5,
            "tools/call",
            json!({"name": "demo.nope", "arguments": {}}),
        ),
        Some("demo.nope"),
    );
    assert_eq!(unknown_tool.status, 200);
    assert_eq!(unknown_tool.json()["error"]["code"], -32602);

    // A dead plugin only exposes <id>.start, exactly like the stdio bridge.
    sandbox.stop_plugin();
    let after_stop = mcp_post(
        &address,
        &token,
        "tools/list",
        &modern_request(6, "tools/list", json!({})),
        None,
    );
    let after_stop = after_stop.json();
    let names = tool_names(&after_stop["result"]["tools"]);
    assert_eq!(names, vec!["demo.start"]);

    // No modern client notification exists in the core protocol; a
    // notification POST is still accepted with an empty 202.
    let notification = mcp_post(
        &address,
        &token,
        "notifications/cancelled",
        br#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#,
        None,
    );
    assert_eq!(notification.status, 202);
    assert!(notification.body.is_empty());

    // Legacy `initialize` is not spoken over HTTP; unknown methods use the
    // standard JSON-RPC error and HTTP 404.
    let initialize = mcp_post(
        &address,
        &token,
        "initialize",
        &modern_request(7, "initialize", json!({})),
        None,
    );
    assert_eq!(initialize.status, 404);
    assert_eq!(initialize.json()["error"]["code"], -32601);
}

#[test]
fn http_mcp_validates_mirrored_headers_and_versions() {
    let (_sandbox, address, token) = http_sandbox(false);
    let json_type = || ("Content-Type".to_string(), "application/json".to_string());

    // The protocol version header is required.
    let body = modern_request(1, "tools/list", json!({}));
    let headers = authorized(
        vec![json_type(), ("Mcp-Method".into(), "tools/list".into())],
        &token,
    );
    let res = http(&address, "POST", "/mcp", &body, &headers);
    assert_header_mismatch(&res);

    // The version header must match the body _meta, not stand alone.
    let headers = authorized(mcp_headers("tools/list", "2025-11-25", None), &token);
    let res = http(&address, "POST", "/mcp", &body, &headers);
    assert_header_mismatch(&res);

    // Mcp-Method is required for every request and must match the body.
    let headers = authorized(
        vec![
            json_type(),
            ("MCP-Protocol-Version".into(), "2026-07-28".into()),
        ],
        &token,
    );
    let res = http(&address, "POST", "/mcp", &body, &headers);
    assert_header_mismatch(&res);

    let headers = authorized(mcp_headers("tools/call", "2026-07-28", None), &token);
    let res = http(&address, "POST", "/mcp", &body, &headers);
    assert_header_mismatch(&res);

    // tools/call mirrors params.name into Mcp-Name.
    let call = modern_request(
        2,
        "tools/call",
        json!({"name": "demo.echo", "arguments": {}}),
    );
    let headers = authorized(mcp_headers("tools/call", "2026-07-28", None), &token);
    let res = http(&address, "POST", "/mcp", &call, &headers);
    assert_header_mismatch(&res);

    let headers = authorized(
        mcp_headers("tools/call", "2026-07-28", Some("demo.other")),
        &token,
    );
    let res = http(&address, "POST", "/mcp", &call, &headers);
    assert_header_mismatch(&res);

    // A version both header and body agree on, but the server does not
    // implement, is UnsupportedProtocolVersionError with the supported list.
    let unsupported = modern_request_versioned(3, "tools/list", json!({}), "1900-01-01");
    let headers = authorized(mcp_headers("tools/list", "1900-01-01", None), &token);
    let res = http(&address, "POST", "/mcp", &unsupported, &headers);
    assert_eq!(res.status, 400, "{:?}", res.body);
    let error = res.json()["error"].clone();
    assert_eq!(error["code"], -32022);
    assert_eq!(error["data"]["supported"], json!(["2026-07-28"]));

    // Missing per-request _meta is the core -32602 case, mapped to 400.
    let no_meta = br#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#;
    let headers = authorized(mcp_headers("tools/list", "2026-07-28", None), &token);
    let res = http(&address, "POST", "/mcp", &no_meta[..], &headers);
    assert_eq!(res.status, 400, "{:?}", res.body);
    assert_eq!(res.json()["error"]["code"], -32602);

    // subscriptions/listen is deliberately not implemented; unknown methods
    // use the transport-level 404 with a MethodNotFoundError body.
    let listen = modern_request(5, "subscriptions/listen", json!({}));
    let headers = authorized(
        mcp_headers("subscriptions/listen", "2026-07-28", None),
        &token,
    );
    let res = http(&address, "POST", "/mcp", &listen, &headers);
    assert_eq!(res.status, 404, "{:?}", res.body);
    assert_eq!(res.json()["error"]["code"], -32601);

    // Malformed JSON is a parse error, not a 200.
    let headers = authorized(mcp_headers("tools/list", "2026-07-28", None), &token);
    let res = http(&address, "POST", "/mcp", b"{not json", &headers);
    assert_eq!(res.status, 400, "{:?}", res.body);
    assert_eq!(res.json()["error"]["code"], -32700);

    // Origin validation: same origin passes, any other origin is forbidden.
    let mut headers = authorized(mcp_headers("tools/list", "2026-07-28", None), &token);
    headers.push(("Origin".into(), format!("http://{}", address)));
    let res = http(&address, "POST", "/mcp", &body, &headers);
    assert_eq!(res.status, 200, "{:?}", res.body);

    let mut headers = authorized(mcp_headers("tools/list", "2026-07-28", None), &token);
    headers.push(("Origin".into(), "http://evil.example".into()));
    let res = http(&address, "POST", "/mcp", &body, &headers);
    assert_eq!(res.status, 403, "{:?}", res.body);
}

#[test]
fn http_mcp_caps_the_request_body_at_one_mib() {
    let (_sandbox, address, token) = http_sandbox(false);
    let headers = authorized(mcp_headers("tools/list", "2026-07-28", None), &token);

    // A larger declared body is rejected from Content-Length alone, before
    // any of it is read (the client never sends the payload).
    let over = http_declared(&address, "/mcp", 1024 * 1024 + 1, &headers);
    assert_eq!(over.status, 413, "{:?}", over.body);

    // Exactly 1 MiB passes the size gate and fails in the JSON parser, which
    // proves the cap is inclusive rather than an off-by-one.
    let body = vec![b' '; 1024 * 1024];
    let exact = http(&address, "POST", "/mcp", &body, &headers);
    assert_eq!(exact.status, 400, "{:?}", exact.body);
    assert_eq!(exact.json()["error"]["code"], -32700);
}
