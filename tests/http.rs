use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
    config: PathBuf,
    data: PathBuf,
    logs: PathBuf,
    trust: PathBuf,
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
            "svcd-http-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let config = root.join("config");
        let data = root.join("data");
        let logs = root.join("logs");
        let trust = config.join("trust.d");
        fs::create_dir_all(&trust).expect("create sandbox");
        fs::create_dir_all(config.join("services.d")).expect("services.d");
        fs::create_dir_all(&data).expect("data");
        fs::create_dir_all(&logs).expect("logs");

        let binary = env!("CARGO_BIN_EXE_svcd");
        let svcd = root.join(if cfg!(windows) { "svcd.exe" } else { "svcd" });
        let svc = root.join(if cfg!(windows) { "svc.exe" } else { "svc" });
        let mcp = root.join(if cfg!(windows) { "mcp.exe" } else { "mcp" });
        for path in [&svcd, &svc, &mcp] {
            fs::copy(binary, path).expect("copy multicall");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
            }
        }

        Sandbox {
            daemon_stderr: root.join("daemon.stderr"),
            root,
            config,
            data,
            logs,
            trust,
            svcd,
            svc,
            mcp,
            daemon: None,
        }
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TRUST_DIR", &self.trust)
            .env("SVCD_HTTP_PORT", "0")
            .env("SVCD_TEST_UNPRIVILEGED", "1");
    }

    /// Send one modern JSON-RPC request to a fresh `mcp` stdio bridge and
    /// return its first response line. One bridge per client matches the
    /// production process model.
    fn mcp_stdio(&self, request: Value) -> Value {
        let mut command = Command::new(&self.mcp);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        self.env(&mut command);
        let mut child = command.spawn().expect("spawn mcp bridge");
        let mut stdin = child.stdin.take().expect("mcp stdin");
        writeln!(
            stdin,
            "{}",
            serde_json::to_string(&request).expect("serialize mcp request")
        )
        .expect("write mcp request");
        stdin.flush().expect("flush mcp request");
        let stdout = child.stdout.take().expect("mcp stdout");
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let _ = sender.send(line);
        });
        let line = receiver
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| {
                let _ = child.kill();
                panic!("mcp bridge did not answer: {}", self.daemon_log())
            });
        let _ = child.kill();
        let _ = child.wait();
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("invalid mcp line {line:?}: {error}"))
    }

    fn run_svc(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(&self.svc);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svc")
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

    /// Like `start_daemon`, but leaves the HTTP port to `svcd.conf` instead of
    /// forcing an ephemeral port through the environment.
    fn start_daemon_without_http_port(&mut self) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("daemon stderr");
        let mut command = Command::new(&self.svcd);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TRUST_DIR", &self.trust)
            .env("SVCD_TEST_UNPRIVILEGED", "1");
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    fn wait_for_http(&self) -> String {
        for _ in 0..200 {
            if let Ok(address) = fs::read_to_string(self.config.join("http.port")) {
                let address = address.trim().to_string();
                if TcpStream::connect(&address).is_ok() {
                    return address;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("HTTP API never became ready: {}", self.daemon_log());
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(&self.daemon_stderr).unwrap_or_default()
    }

    fn shutdown(&mut self) {
        if self.daemon.is_some() {
            let mut command = Command::new(&self.svcd);
            command.arg("shutdown");
            self.env(&mut command);
            let _ = command.output();
            if let Some(mut child) = self.daemon.take() {
                let _ = child.wait();
            }
        }
    }

    fn write_core(&self) {
        let script = if cfg!(windows) {
            self.root.join("core.cmd")
        } else {
            self.root.join("core.sh")
        };
        let text = if cfg!(windows) {
            "@echo off\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\n"
        } else {
            "#!/bin/sh\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n"
        };
        fs::write(&script, text).expect("write core script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let (path, args) = if cfg!(windows) {
            (
                r"C:\Windows\System32\cmd.exe".to_string(),
                vec!["/c".to_string(), script.to_string_lossy().to_string()],
            )
        } else {
            (
                "/bin/sh".to_string(),
                vec![script.to_string_lossy().to_string()],
            )
        };
        let definition = json!({
            "schema": 1,
            "id": "demo",
            "version": "1.0.0",
            "desc": "core demo",
            "protocol": 1,
            "exec": {"path": path, "args": args, "ready": "notify"}
        });
        fs::write(
            self.config.join("services.d").join("demo.json"),
            serde_json::to_vec(&definition).expect("definition"),
        )
        .expect("write core definition");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Response {
    status: u16,
    body: Value,
    headers: Vec<(String, String)>,
}

struct RawResponse {
    status: u16,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

fn request(address: &str, method: &str, path: &str, content_type: &str, body: &[u8]) -> Response {
    request_with(address, method, path, content_type, body, &[])
}

fn request_with(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> Response {
    let raw = raw_request_with(address, method, path, content_type, body, extra_headers);
    let body = serde_json::from_slice(&raw.body).expect("json body");
    Response {
        status: raw.status,
        body,
        headers: raw.headers,
    }
}

fn raw_request(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> RawResponse {
    raw_request_with(address, method, path, content_type, body, &[])
}

fn request_head(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    content_length: usize,
    extra_headers: &[(&str, &str)],
) -> String {
    let mut request = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        method, path, address, content_length
    );
    if !content_type.is_empty() {
        request.push_str(&format!("Content-Type: {}\r\n", content_type));
    }
    for (name, value) in extra_headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    request
}

fn read_raw_response(stream: &mut TcpStream) -> RawResponse {
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers");
    let head = String::from_utf8_lossy(&response[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("status");
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    RawResponse {
        status,
        body: response[split + 4..].to_vec(),
        headers,
    }
}

fn read_one_response(stream: &mut TcpStream) -> RawResponse {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("status");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body).expect("response body");
    RawResponse {
        status,
        body,
        headers,
    }
}

fn raw_request_with(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> RawResponse {
    raw_request_paced(
        address,
        method,
        path,
        content_type,
        body,
        extra_headers,
        None,
    )
}

/// Send the request headers, pause, then send the body. A server that reads
/// the body from a non-blocking accepted socket sees `WouldBlock` during the
/// pause instead of waiting for the rest of the request.
fn raw_request_split_body(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    header_to_body: Duration,
) -> RawResponse {
    raw_request_paced(
        address,
        method,
        path,
        content_type,
        body,
        &[],
        Some(header_to_body),
    )
}

fn raw_request_paced(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
    header_to_body: Option<Duration>,
) -> RawResponse {
    let mut stream = TcpStream::connect(address).expect("connect");
    let request = request_head(
        address,
        method,
        path,
        content_type,
        body.len(),
        extra_headers,
    );
    stream.write_all(request.as_bytes()).expect("write headers");
    if let Some(pause) = header_to_body {
        std::thread::sleep(pause);
    }
    stream.write_all(body).expect("write body");
    read_raw_response(&mut stream)
}

fn json_body(response: &RawResponse) -> Value {
    serde_json::from_slice(&response.body).expect("json body")
}

fn header_values<'a>(response: &'a Response, name: &str) -> Vec<&'a str> {
    response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

fn raw_header_values<'a>(response: &'a RawResponse, name: &str) -> Vec<&'a str> {
    response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

fn cookie_value(response: &Response, name: &str) -> Option<String> {
    header_values(response, "set-cookie")
        .into_iter()
        .find_map(|value| {
            let (pair, _) = value.split_once(';')?;
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_string())
        })
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

/// One modern MCP request over Streamable HTTP. `name` mirrors
/// `params.name` into `Mcp-Name` for tools/call.
fn mcp_request(
    address: &str,
    token: &str,
    method: &str,
    params: Value,
    name: Option<&str>,
) -> Response {
    let mut params = params;
    if !params.is_object() {
        params = json!({});
    }
    params["_meta"] = modern_meta();
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("serialize mcp request");
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {}", token)),
        ("MCP-Protocol-Version".to_string(), "2026-07-28".to_string()),
        ("Mcp-Method".to_string(), method.to_string()),
    ];
    if let Some(name) = name {
        headers.push(("Mcp-Name".to_string(), name.to_string()));
    }
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    request_with(address, "POST", "/mcp", "application/json", &body, &headers)
}

fn tool_names(tools: &Value) -> Vec<String> {
    tools
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

fn script() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "agent.cmd",
            b"@echo off\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\n".to_vec(),
        )
    } else {
        (
            "agent.sh",
            b"#!/bin/sh\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n".to_vec(),
        )
    }
}

fn plugin_package(key: &SigningKey) -> Vec<u8> {
    plugin_bundle(key, "demo", "1.0.0", None, false)
}

fn plugin_bundle(
    key: &SigningKey,
    id: &str,
    version: &str,
    config: Option<&[u8]>,
    check: bool,
) -> Vec<u8> {
    let (name, data) = script();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![(exec_path.clone(), data, 0o755)];
    if let Some(config) = config {
        files.push(("config/config".to_string(), config.to_vec(), 0o644));
    }
    let check_path = if check {
        let (path, body) = if cfg!(windows) {
            (
                "check/check.cmd",
                b"@echo off\r\nfindstr /C:\"good\" \"%SVCD_CONFIG%\" >nul\r\nexit /b %errorlevel%\r\n".to_vec(),
            )
        } else {
            (
                "check/check.sh",
                b"#!/bin/sh\ngrep -q good \"$SVCD_CONFIG\"\n".to_vec(),
            )
        };
        files.push((path.to_string(), body, 0o755));
        Some(path)
    } else {
        None
    };
    files.sort_by(|left, right| left.0.cmp(&right.0));

    let mut manifest = json!({
        "schema": 1,
        "name": id,
        "version": version,
        "desc": "http plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
    });
    if config.is_some() {
        manifest["config"] = json!("config/config");
    }
    if let Some(path) = check_path {
        manifest["configCheck"] = json!({"path": path});
    }
    signed_package(key, manifest, &files)
}

/// Attach \`files[]\`, sign the manifest and wrap everything in the ustar layout
/// the installer verifies. Shared by the plain and web-UI fixtures.
fn signed_package(
    key: &SigningKey,
    mut manifest: Value,
    files: &[(String, Vec<u8>, u32)],
) -> Vec<u8> {
    manifest["files"] = json!(files
        .iter()
        .map(|(path, data, mode)| json!({
            "path": path,
            "size": data.len(),
            "sha256": hex_sha256(data),
            "mode": mode
        }))
        .collect::<Vec<Value>>());
    let manifest = serde_json::to_vec(&manifest).expect("manifest");
    let signature =
        json!({"alg":"ed25519","keyId":"test","sig":BASE64.encode(key.sign(&manifest).to_bytes())});
    let signature = serde_json::to_vec(&signature).expect("signature");
    let mut tar = Vec::new();
    tar.extend_from_slice(&header("manifest.json", manifest.len(), 0o644));
    tar.extend_from_slice(&manifest);
    tar.extend(std::iter::repeat_n(0, padding(manifest.len())));
    tar.extend_from_slice(&header("signature.json", signature.len(), 0o644));
    tar.extend_from_slice(&signature);
    tar.extend(std::iter::repeat_n(0, padding(signature.len())));
    for (path, data, mode) in files {
        tar.extend_from_slice(&header(path, data.len(), *mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

/// A plugin-side NDJSON agent that answers \`svcd.ping\` and \`tools/call\` with a
/// real tool result, echoing the request id. The sh variant extracts the id with
/// sed; the PowerShell variant uses a regex.
fn web_agent() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "agent.ps1",
            br#"$ErrorActionPreference = 'Stop'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $id = 1
  if ($line -match '"id"\s*:\s*(\d+)') { $id = [int]$Matches[1] }
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}')
  [Console]::Out.Flush()
}
"#
            .to_vec(),
        )
    } else {
        (
            "agent.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=1; fi
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id"
done
"#
            .to_vec(),
        )
    }
}

/// A plugin with \`payload/web/\` assets and one declared capability. \`ui\`
/// controls whether \`manifest.ui\` declares a browser UI at all.
fn web_plugin_bundle(key: &SigningKey, id: &str, ui: bool) -> Vec<u8> {
    let (name, data) = web_agent();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), data, 0o755),
        (
            "config/config".to_string(),
            b"initial config\n".to_vec(),
            0o644,
        ),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><head><title>demo ui</title></head><body>plugin page</body></html>"
                .to_vec(),
            0o644,
        ),
        (
            "payload/web/app.js".to_string(),
            b"export const answer = 42;\n".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut manifest = json!({
        "schema": 1,
        "name": id,
        "version": "1.0.0",
        "desc": "web ui plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "capabilities": [
            {"name": "status", "desc": "sample status", "inputSchema": {"type": "object"}}
        ],
    });
    manifest["config"] = json!("config/config");
    if ui {
        manifest["ui"] = json!({"type": "web", "entry": "index.html"});
    }
    signed_package(key, manifest, &files)
}

/// A web UI plugin whose agent emits a notification before every response.
fn web_event_agent() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "event-agent.ps1",
            br#"$ErrorActionPreference = 'Stop'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  $id = 1
  if ($line -match '"id"\s*:\s*(\d+)') { $id = [int]$Matches[1] }
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"pong-event"}}')
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}')
  [Console]::Out.Flush()
}
"#
            .to_vec(),
        )
    } else {
        (
            "event-agent.sh",
            br#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then id=1; fi
  printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"pong-event"}}\n'
  printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id"
done
"#
            .to_vec(),
        )
    }
}

fn web_event_plugin_bundle(key: &SigningKey, id: &str) -> Vec<u8> {
    let (name, data) = web_event_agent();
    let exec_path = format!("payload/bin/{}", name);
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), data, 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>events</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "name": id,
        "version": "1.0.0",
        "desc": "event ui plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "capabilities": [
            {"name": "status", "desc": "sample status", "inputSchema": {"type": "object"}}
        ],
        "ui": {"type": "web", "entry": "index.html"},
    });
    signed_package(key, manifest, &files)
}

fn daemon_with_web_event_plugin(key_byte: u8) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_event_plugin_bundle(&key, "demo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    (sandbox, address)
}

fn ws_key() -> String {
    BASE64.encode([0x42u8; 16])
}

/// Open a WebSocket handshake and return (stream, status, response head).
fn ws_handshake(address: &str, path: &str, extra: &[(&str, &str)]) -> (TcpStream, u16, String) {
    let mut stream = TcpStream::connect(address).expect("ws connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("ws timeout");
    let key = ws_key();
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        path, address, key
    );
    for (name, value) in extra {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("ws request");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("ws response head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("ws status");
    (stream, status, head)
}

fn ws_send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![0x80 | opcode];
    if payload.len() < 126 {
        frame.push(0x80 | payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&frame).expect("ws frame");
    stream.flush().expect("ws flush");
}

fn ws_read(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).expect("ws frame head");
    let opcode = header[0] & 0x0f;
    assert_eq!(header[1] & 0x80, 0, "server frames must be unmasked");
    let mut length = u64::from(header[1] & 0x7f);
    if length == 126 {
        let mut extended = [0u8; 2];
        stream.read_exact(&mut extended).expect("ws length");
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0u8; 8];
        stream.read_exact(&mut extended).expect("ws length");
        length = u64::from_be_bytes(extended);
    }
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload).expect("ws payload");
    (opcode, payload)
}

fn sse_connect(address: &str, path: &str) -> TcpStream {
    sse_connect_with_last_id(address, path, None)
}

fn sse_connect_with_last_id(address: &str, path: &str, last_id: Option<u64>) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("sse connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sse timeout");
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
        path, address
    );
    if let Some(last_id) = last_id {
        request.push_str(&format!("Last-Event-ID: {}\r\n", last_id));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("sse request");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("sse response head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 200"), "{}", head);
    assert!(
        head.to_ascii_lowercase().contains("text/event-stream"),
        "{}",
        head
    );
    stream
}

fn sse_id(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("id: "))
        .and_then(|value| value.trim().parse::<u64>().ok())
}

fn sse_wait_for_data(stream: &mut TcpStream, needle: &str) -> String {
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).expect("sse data");
        seen.push(byte[0]);
        if seen.ends_with(b"\n\n") {
            let text = String::from_utf8_lossy(&seen).to_string();
            if text.contains(needle) {
                return text;
            }
            seen.clear();
        }
    }
}

/// Start a daemon and install the `demo` plugin (with a config template)
/// under a fresh trust key. Returns the sandbox and its HTTP address.
fn daemon_with_demo_plugin(key_byte: u8) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle(&key, "demo", "1.0.0", Some(b"good template\n"), true);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

/// A running daemon with the demo web-UI plugin installed.
fn web_ui_sandbox(key_byte: u8) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_plugin_bundle(&key, "demo", true);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    (sandbox, address)
}

fn tar_names(bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut offset = 0;
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let end = header[..100]
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(100);
        names.push(String::from_utf8_lossy(&header[..end]).to_string());
        let size = std::str::from_utf8(&header[124..135])
            .ok()
            .and_then(|value| {
                usize::from_str_radix(value.trim_matches(char::from(0)).trim(), 8).ok()
            })
            .unwrap_or(0);
        offset += 512 + size + padding(size);
    }
    names
}

fn header(path: &str, size: usize, mode: u32) -> [u8; 512] {
    let mut header = [0u8; 512];
    header[..path.len()].copy_from_slice(path.as_bytes());
    let mode = format!("{:07o}", mode);
    header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
    let size = format!("{:011o}", size);
    header[124..124 + size.len()].copy_from_slice(size.as_bytes());
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header
}

fn padding(size: usize) -> usize {
    let remainder = size % 512;
    if remainder == 0 {
        0
    } else {
        512 - remainder
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

fn pem_from_public(key: &[u8; 32]) -> Vec<u8> {
    const PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(key);
    let body = BASE64.encode(der);
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    pem.into_bytes()
}

#[test]
fn services_version_start_and_stop() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(services.status, 200);
    let ids: Vec<&str> = services.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .filter_map(|service| service["id"].as_str())
        .collect();
    assert!(ids.contains(&"svc"), "{ids:?}");
    assert!(ids.contains(&"demo"), "{ids:?}\n{}", sandbox.daemon_log());

    let status = request(&address, "GET", "/api/v1/services/demo", "", b"");
    assert_eq!(status.status, 200);
    assert_eq!(status.body["summary"]["kind"], "core");

    let version = request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(version.status, 200);
    assert_eq!(version.body["protocol"], 1);
    assert!(version.body["platform"].is_string());

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(started.body["summary"]["state"], "running");

    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    assert_eq!(stopped.body["summary"]["state"], "stopped");

    let missing = request(&address, "GET", "/api/v1/services/missing", "", b"");
    assert_eq!(missing.status, 404);
    assert!(missing.body["error"]["code"].is_string());
    assert!(missing.body["error"]["message"].is_string());
}

#[test]
fn plugin_upload_list_and_delete() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[21u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_package(&key);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins?strategy=keep-old",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(installed.body["id"], "demo");
    assert_eq!(installed.body["version"], "1.0.0");

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let plugin = services.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .find(|service| service["id"] == "demo")
        .expect("plugin service");
    assert_eq!(plugin["kind"], "plugin");

    let disabled = request(&address, "POST", "/api/v1/services/demo/disable", "", b"");
    assert_eq!(disabled.status, 200, "{}", disabled.body);
    assert_eq!(disabled.body["summary"]["enabled"], false);
    let enabled = request(&address, "POST", "/api/v1/services/demo/enable", "", b"");
    assert_eq!(enabled.status, 200, "{}", enabled.body);
    assert_eq!(enabled.body["summary"]["enabled"], true);
    let core_toggle = request(&address, "POST", "/api/v1/services/svc/disable", "", b"");
    assert_eq!(core_toggle.status, 409, "{}", core_toggle.body);

    let deleted = request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(deleted.status, 200, "{}", deleted.body);

    let core_delete = request(&address, "DELETE", "/api/v1/plugins/svc", "", b"");
    assert_eq!(core_delete.status, 409, "{}", core_delete.body);
    assert_eq!(core_delete.body["error"]["code"], "conflict");

    let after = request(&address, "GET", "/api/v1/services", "", b"");
    assert!(!after.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .any(|service| service["id"] == "demo"));

    let mut bad = package.clone();
    let needle = b"\"demo\"";
    let position = bad
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("name");
    bad[position + 1] = b'D';
    let bad_upload = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &bad,
    );
    assert_eq!(bad_upload.status, 400, "{}", bad_upload.body);

    let wrong_type = request(&address, "POST", "/api/v1/plugins", "text/plain", &package);
    assert_eq!(wrong_type.status, 400);
    assert_eq!(wrong_type.body["error"]["code"], "bad_request");
}

#[test]
fn configured_port_and_paths_come_from_svcd_conf() {
    let mut sandbox = Sandbox::new();
    let plugins = sandbox.root.join("configured-plugins");
    let services = sandbox.root.join("configured-services");
    fs::create_dir_all(&services).expect("configured services");
    let (exec, args) = if cfg!(windows) {
        (r"C:\Windows\System32\cmd.exe", json!(["/c", "exit", "0"]))
    } else {
        ("/bin/true", json!([]))
    };
    fs::write(
        services.join("custom.json"),
        serde_json::to_vec(&json!({
            "schema": 1,
            "id": "custom",
            "version": "1.0.0",
            "desc": "configured core service",
            "protocol": 1,
            "exec": {"path": exec, "args": args, "ready": "exec"}
        }))
        .expect("core definition"),
    )
    .expect("write core definition");
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!(
            "http_port = 0\nplugins_dir = {}\nservices_dir = {}\n",
            plugins.display(),
            services.display()
        ),
    )
    .expect("write svcd.conf");

    let key = SigningKey::from_bytes(&[23u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_package(&key);

    sandbox.start_daemon_without_http_port();
    let address = sandbox.wait_for_http();

    let services_response = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(services_response.status, 200);
    let ids: Vec<&str> = services_response.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .filter_map(|service| service["id"].as_str())
        .collect();
    assert!(ids.contains(&"custom"), "{ids:?}");

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins?strategy=replace",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert!(
        plugins.join("demo").join("1.0.0").is_dir(),
        "plugin did not use plugins_dir"
    );
    assert!(
        !sandbox.data.join("plugins").join("demo").exists(),
        "plugin used the default data directory"
    );
}

#[test]
fn oversized_upload_is_rejected_before_the_body() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let mut stream = TcpStream::connect(&address).expect("connect");
    let request = format!(
        "POST /api/v1/plugins HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        512u64 * 1024 * 1024 + 1
    );
    stream.write_all(request.as_bytes()).expect("write headers");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    let head = String::from_utf8_lossy(&response);
    assert!(head.starts_with("HTTP/1.1 413"), "{}", head);
}

fn token_line(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| {
            line.strip_prefix("token: ")
                .map(str::trim)
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("no token in output: {}", text))
}

fn plugin_dir(sandbox: &Sandbox, id: &str) -> PathBuf {
    sandbox.data.join("plugins").join(id)
}

#[test]
fn config_endpoints_read_write_reset_and_reject_bad_checks() {
    let (sandbox, address) = daemon_with_demo_plugin(31);
    let directory = plugin_dir(&sandbox, "demo");
    let active = directory.join("config");
    assert_eq!(fs::read(&active).expect("config"), b"good template\n");

    let read = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(read.status, 200);
    assert_eq!(read.body, b"good template\n");
    assert_eq!(raw_header_values(&read, "content-type"), vec!["text/plain"]);

    let good = raw_request(
        &address,
        "PUT",
        "/api/v1/config/demo",
        "text/plain",
        b"good config\n",
    );
    assert_eq!(good.status, 200, "{}", String::from_utf8_lossy(&good.body));
    assert_eq!(fs::read(&active).expect("config"), b"good config\n");
    assert_eq!(
        fs::read(directory.join("config.last-good")).expect("last-good"),
        b"good template\n"
    );

    let bad = raw_request(
        &address,
        "PUT",
        "/api/v1/config/demo",
        "text/plain",
        b"bad config\n",
    );
    assert_eq!(bad.status, 409, "{}", String::from_utf8_lossy(&bad.body));
    assert_eq!(fs::read(&active).expect("config"), b"good config\n");

    let reset = raw_request(&address, "POST", "/api/v1/config/demo/reset", "", b"");
    assert_eq!(
        reset.status,
        200,
        "{}",
        String::from_utf8_lossy(&reset.body)
    );
    assert_eq!(fs::read(&active).expect("config"), b"good template\n");
    assert_eq!(
        fs::read(directory.join("config.last-good")).expect("last-good"),
        b"good config\n"
    );

    let bad_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"bad config\n",
    );
    assert_eq!(
        bad_import.status,
        409,
        "{}",
        String::from_utf8_lossy(&bad_import.body)
    );

    let too_large = vec![b'x'; 1024 * 1024 + 1];
    let large = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        &too_large,
    );
    assert_eq!(
        large.status,
        413,
        "{}",
        String::from_utf8_lossy(&large.body)
    );
}

#[test]
fn a_large_body_sent_after_its_headers_is_read_to_completion() {
    let (_sandbox, address) = daemon_with_demo_plugin(32);

    // A small body split from its headers binds the assertion to the socket
    // behaviour: the broken non-blocking socket answered 400 "cannot read
    // request body" instead of waiting. The 300ms pause is deliberately
    // generous; the in-process daemon parses the headers in microseconds, so
    // it is already inside the body read when the rest arrives.
    let small = raw_request_split_body(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"good config\n",
        Duration::from_millis(300),
    );
    assert_eq!(
        small.status,
        200,
        "{}",
        String::from_utf8_lossy(&small.body)
    );

    // The oversized body must be rejected by the config cap, not by a read
    // error. This is the ticket's 1 MiB + 1 case.
    let too_large = vec![b'x'; 1024 * 1024 + 1];
    let response = raw_request_split_body(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        &too_large,
        Duration::from_millis(300),
    );
    assert_eq!(
        response.status,
        413,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
}

#[test]
fn export_import_and_logs() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[32u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle(&key, "demo", "1.0.0", Some(b"good template\n"), false);
    let plain = plugin_bundle(&key, "plain", "1.0.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    assert_eq!(
        request(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            &package
        )
        .status,
        200
    );
    assert_eq!(
        request(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            &plain
        )
        .status,
        200
    );

    let directory = plugin_dir(&sandbox, "demo");
    let active = directory.join("config");

    let spkb = raw_request(&address, "GET", "/api/v1/export/demo", "", b"");
    assert_eq!(spkb.status, 200, "{}", String::from_utf8_lossy(&spkb.body));
    assert_eq!(
        raw_header_values(&spkb, "content-type"),
        vec!["application/octet-stream"]
    );
    assert!(raw_header_values(&spkb, "content-disposition")[0].contains("demo-1.0.0.spkb"));
    let names = tar_names(&spkb.body);
    assert_eq!(names.first().map(String::as_str), Some("backup.json"));
    assert!(names.iter().any(|name| name == "plugin.spk"), "{names:?}");
    assert!(names.iter().any(|name| name == "config"), "{names:?}");

    let config_export = raw_request(&address, "GET", "/api/v1/export/demo/config", "", b"");
    assert_eq!(config_export.status, 200);
    assert_eq!(config_export.body, b"good template\n");

    let raw_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"imported good\n",
    );
    assert_eq!(
        raw_import.status,
        200,
        "{}",
        String::from_utf8_lossy(&raw_import.body)
    );
    assert_eq!(json_body(&raw_import)["configApplied"], true);
    assert_eq!(fs::read(&active).expect("config"), b"imported good\n");

    let missing_id = raw_request(&address, "POST", "/api/v1/import", "text/plain", b"no id\n");
    assert_eq!(missing_id.status, 400);

    let backup_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &spkb.body,
    );
    assert_eq!(
        backup_import.status,
        200,
        "{}",
        String::from_utf8_lossy(&backup_import.body)
    );
    let backup_body = json_body(&backup_import);
    assert_eq!(backup_body["id"], "demo");
    assert_eq!(backup_body["configApplied"], true);
    assert_eq!(fs::read(&active).expect("config"), b"good template\n");

    let spk_import = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &package,
    );
    assert_eq!(
        spk_import.status,
        200,
        "{}",
        String::from_utf8_lossy(&spk_import.body)
    );
    assert_eq!(json_body(&spk_import)["id"], "demo");

    let mut bad = package.clone();
    let needle = b"\"demo\"";
    let position = bad
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("name");
    bad[position + 1] = b'D';
    let bad_import = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &bad,
    );
    assert_eq!(
        bad_import.status,
        400,
        "{}",
        String::from_utf8_lossy(&bad_import.body)
    );

    let no_config = raw_request(&address, "GET", "/api/v1/config/plain", "", b"");
    assert_eq!(no_config.status, 404);

    fs::write(sandbox.logs.join("demo.log"), "one\ntwo\nthree\n").expect("write log");
    let tail = raw_request(&address, "GET", "/api/v1/logs/demo?lines=2", "", b"");
    assert_eq!(tail.status, 200);
    assert_eq!(tail.body, b"two\nthree\n");
    assert_eq!(
        raw_header_values(&tail, "content-type"),
        vec!["text/plain; charset=utf-8"]
    );
    let too_many = raw_request(&address, "GET", "/api/v1/logs/demo?lines=2001", "", b"");
    assert_eq!(too_many.status, 400);
    let no_log = raw_request(&address, "GET", "/api/v1/logs/plain", "", b"");
    assert_eq!(no_log.status, 404);

    let big = "x".repeat(1024 * 1024 + 100);
    fs::write(sandbox.logs.join("demo.log"), &big).expect("write big log");
    let capped = raw_request(&address, "GET", "/api/v1/logs/demo?lines=200", "", b"");
    assert_eq!(capped.status, 200);
    assert!(
        capped.body.len() <= 1024 * 1024,
        "log response was {} bytes",
        capped.body.len()
    );
}

#[test]
fn config_endpoints_are_plugin_only_and_enforce_methods() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    for (method, path) in [
        ("GET", "/api/v1/config/demo"),
        ("PUT", "/api/v1/config/demo"),
        ("POST", "/api/v1/config/demo/reset"),
        ("GET", "/api/v1/export/demo"),
        ("GET", "/api/v1/export/demo/config"),
        ("GET", "/api/v1/logs/demo"),
    ] {
        let response = raw_request(&address, method, path, "", b"");
        assert_eq!(
            response.status,
            404,
            "{} {} -> {}",
            method,
            path,
            String::from_utf8_lossy(&response.body)
        );
    }

    let import_core = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"good\n",
    );
    assert_eq!(
        import_core.status,
        404,
        "{}",
        String::from_utf8_lossy(&import_core.body)
    );

    let key = SigningKey::from_bytes(&[33u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let core_package = plugin_bundle(&key, "demo", "1.0.0", None, false);
    let core_import = raw_request(
        &address,
        "POST",
        "/api/v1/import",
        "application/octet-stream",
        &core_package,
    );
    assert_eq!(
        core_import.status,
        404,
        "{}",
        String::from_utf8_lossy(&core_import.body)
    );

    let reset_get = raw_request(&address, "GET", "/api/v1/config/demo/reset", "", b"");
    assert_eq!(reset_get.status, 405);
    assert_eq!(raw_header_values(&reset_get, "allow"), vec!["POST"]);

    let config_post = raw_request(&address, "POST", "/api/v1/config/demo", "", b"");
    assert_eq!(config_post.status, 405);
    assert_eq!(raw_header_values(&config_post, "allow"), vec!["GET, PUT"]);

    let export_post = raw_request(&address, "POST", "/api/v1/export/demo", "", b"");
    assert_eq!(export_post.status, 405);
    assert_eq!(raw_header_values(&export_post, "allow"), vec!["GET"]);

    let import_get = raw_request(&address, "GET", "/api/v1/import", "", b"");
    assert_eq!(import_get.status, 405);
}

#[test]
fn serves_the_embedded_console() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let page = raw_request(&address, "GET", "/", "", b"");
    assert_eq!(page.status, 200);
    assert_eq!(
        raw_header_values(&page, "content-type"),
        vec!["text/html; charset=utf-8"]
    );
    let html = String::from_utf8_lossy(&page.body);
    assert!(html.contains("id=\"rows\""), "console table missing");
    assert!(
        html.contains("/api/v1/services"),
        "console does not call the management API"
    );
    let index = raw_request(&address, "GET", "/index.html", "", b"");
    assert_eq!(index.status, 200);

    let post = raw_request(&address, "POST", "/", "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET"]);
}

#[test]
fn lan_auth_bearer_cookie_csrf_rotate_and_disable() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let open = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(open.status, 200);

    let enabled = sandbox.run_svc(&["lan", "enable", "--bind", "0.0.0.0"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    let token = token_line(&enabled);
    assert!(sandbox.config.join("lan-token").is_file());
    assert!(sandbox.config.join("lan.json").is_file());

    let page = raw_request(&address, "GET", "/", "", b"");
    assert_eq!(page.status, 200, "console must load before login");

    let denied = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(denied.status, 401);
    assert_eq!(denied.body["error"]["code"], "unauthorized");

    let bearer = format!("Bearer {}", token);
    let allowed = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(allowed.status, 200, "{}", allowed.body);

    let login = request_with(
        &address,
        "POST",
        "/login",
        "application/json",
        serde_json::to_string(&json!({ "token": token }))
            .expect("login")
            .as_bytes(),
        &[],
    );
    assert_eq!(login.status, 200, "{}", login.body);
    let cookie = cookie_value(&login, "svcd_api").expect("svcd_api cookie");
    let session = cookie_value(&login, "svcd_session").expect("svcd_session cookie");
    let cookies = header_values(&login, "set-cookie").join("\n");
    assert!(cookies.contains("Path=/api/v1/"), "{cookies}");
    assert!(cookies.contains("Path=/plugin-ui/"), "{cookies}");
    assert!(cookies.contains("HttpOnly"), "{cookies}");
    assert!(cookies.contains("SameSite=Strict"), "{cookies}");
    assert!(cookies.contains("Max-Age=86400"), "{cookies}");
    assert!(!session.is_empty());

    let api_cookie = format!("svcd_api={}", cookie);
    let cookie_get = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(cookie_get.status, 200);

    let no_csrf = request_with(
        &address,
        "POST",
        "/api/v1/services/svc/stop",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(no_csrf.status, 403);

    let origin = format!("http://{}", address);
    let csrf = request_with(
        &address,
        "POST",
        "/api/v1/services/svc/stop",
        "",
        b"",
        &[
            ("Cookie", api_cookie.as_str()),
            ("Origin", origin.as_str()),
            ("X-Svcd-Csrf", "1"),
        ],
    );
    assert_eq!(csrf.status, 200, "{}", csrf.body);

    let lan = request_with(
        &address,
        "GET",
        "/api/v1/lan",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(lan.status, 200);
    assert_eq!(lan.body["enabled"], true);

    let logout = request_with(
        &address,
        "POST",
        "/logout",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(logout.status, 200);
    let after_logout = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Cookie", api_cookie.as_str())],
    );
    assert_eq!(after_logout.status, 401);

    let rotated = sandbox.run_svc(&["lan", "token", "rotate"]);
    assert_eq!(rotated.status.code(), Some(0));
    let new_token = token_line(&rotated);
    let old_bearer = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(old_bearer.status, 401);
    let new_bearer = format!("Bearer {}", new_token);
    let new_allowed = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", new_bearer.as_str())],
    );
    assert_eq!(new_allowed.status, 200);

    let status = sandbox.run_svc(&["lan", "status"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&status.stdout).contains("enabled: true"));

    sandbox.shutdown();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let denied_after_restart = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(denied_after_restart.status, 401);
    let new_bearer = format!("Bearer {}", new_token);
    let allowed_after_restart = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", new_bearer.as_str())],
    );
    assert_eq!(allowed_after_restart.status, 200);

    let disabled = sandbox.run_svc(&["lan", "disable"]);
    assert_eq!(disabled.status.code(), Some(0));
    let open_again = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(open_again.status, 200);
}
#[test]
fn plugin_web_ui_serves_assets_with_base_and_etag() {
    let (_sandbox, address) = web_ui_sandbox(41);
    let key = SigningKey::from_bytes(&[41u8; 32]);
    let plain = web_plugin_bundle(&key, "plain", false);
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &plain,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);
    assert_eq!(installed.body["id"], "plain");

    let page = raw_request(&address, "GET", "/plugin-ui/demo/", "", b"");
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert_eq!(
        raw_header_values(&page, "content-type"),
        vec!["text/html; charset=utf-8"]
    );
    assert_eq!(raw_header_values(&page, "cache-control"), vec!["no-cache"]);
    assert_eq!(raw_header_values(&page, "vary"), vec!["Accept"]);
    assert_eq!(
        raw_header_values(&page, "x-content-type-options"),
        vec!["nosniff"]
    );
    let html = String::from_utf8_lossy(&page.body);
    assert!(
        html.starts_with("<!doctype html>\n<base href=\"/plugin-ui/demo/\">\n"),
        "{html}"
    );
    assert!(html.contains("plugin page"), "{html}");

    let etags = raw_header_values(&page, "etag");
    assert_eq!(etags.len(), 1, "{:?}", page.headers);
    let etag = etags[0].to_string();
    let revalidated = raw_request_with(
        &address,
        "GET",
        "/plugin-ui/demo/",
        "",
        b"",
        &[("If-None-Match", etag.as_str())],
    );
    assert_eq!(revalidated.status, 304);
    assert!(revalidated.body.is_empty());
    assert_eq!(raw_header_values(&revalidated, "etag"), vec![etag.as_str()]);

    let script = raw_request(&address, "GET", "/plugin-ui/demo/app.js", "", b"");
    assert_eq!(script.status, 200);
    assert_eq!(
        raw_header_values(&script, "content-type"),
        vec!["text/javascript"]
    );
    assert_eq!(script.body, b"export const answer = 42;\n");

    // The canonical URL has a trailing slash so the /plugin-ui/ session cookie
    // is actually sent by browsers when LAN auth is on.
    let no_slash = raw_request(&address, "GET", "/plugin-ui/demo", "", b"");
    assert_eq!(no_slash.status, 308);
    assert_eq!(
        raw_header_values(&no_slash, "location"),
        vec!["/plugin-ui/demo/"]
    );

    // SPA fallback only for HTML navigations; a missing asset stays a 404.
    let missing_asset = raw_request(&address, "GET", "/plugin-ui/demo/missing.js", "", b"");
    assert_eq!(missing_asset.status, 404);
    let spa = raw_request_with(
        &address,
        "GET",
        "/plugin-ui/demo/settings/tunnel",
        "",
        b"",
        &[("Accept", "text/html")],
    );
    assert_eq!(spa.status, 200);
    assert!(String::from_utf8_lossy(&spa.body).contains("plugin page"));

    for path in [
        "/plugin-ui/demo/%2e%2e/secret",
        "/plugin-ui/demo/../secret",
        "/plugin-ui/..%2fsecret",
        "/plugin-ui/Demo/",
        "/plugin-ui/missing/",
    ] {
        let denied = raw_request(&address, "GET", path, "", b"");
        assert_eq!(denied.status, 404, "{path}");
        // A rejected path is not a client-side route: an HTML navigation must
        // not turn traversal into the SPA entry.
        let denied_html =
            raw_request_with(&address, "GET", path, "", b"", &[("Accept", "text/html")]);
        assert_eq!(denied_html.status, 404, "{path} with Accept: text/html");
    }

    // A plugin without manifest.ui has no route even if payload/web exists.
    let no_ui = raw_request(&address, "GET", "/plugin-ui/plain/", "", b"");
    assert_eq!(no_ui.status, 404);

    let post = raw_request(&address, "POST", "/plugin-ui/demo/", "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET"]);
}

fn rpc_body(id: u64, name: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": {}}
    }))
    .expect("rpc body")
}

#[test]
fn plugin_web_ui_rpc_bridges_tools_call() {
    let (_sandbox, address) = web_ui_sandbox(42);

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);

    let call = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &rpc_body(7, "status"),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["jsonrpc"], "2.0");
    assert_eq!(call.body["id"], 7);
    assert_eq!(call.body["result"]["content"][0]["text"], "pong");
    assert_eq!(call.body["result"]["isError"], false);

    // A fully qualified capability name is accepted too.
    let prefixed = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &rpc_body(8, "demo.status"),
    );
    assert_eq!(prefixed.body["id"], 8);
    assert_eq!(prefixed.body["result"]["content"][0]["text"], "pong");

    // Same-origin Origin is accepted; a foreign one is rejected.
    let origin = format!("http://{}", address);
    let body = rpc_body(9, "status");
    let same_origin = request_with(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &body,
        &[("Origin", origin.as_str())],
    );
    assert_eq!(same_origin.status, 200, "{}", same_origin.body);
    let cross_origin = request_with(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &body,
        &[("Origin", "http://evil.example")],
    );
    assert_eq!(cross_origin.status, 403);

    let unsupported = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        br#"{"jsonrpc":"2.0","id":10,"method":"svcd.ping"}"#,
    );
    assert_eq!(unsupported.status, 200);
    assert_eq!(unsupported.body["id"], 10);
    assert_eq!(unsupported.body["error"]["code"], -32601);

    let malformed = raw_request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        b"{",
    );
    assert_eq!(malformed.status, 200);
    assert_eq!(json_body(&malformed)["error"]["code"], -32700);

    let unknown = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &rpc_body(11, "nope"),
    );
    assert_eq!(unknown.body["id"], 11);
    assert_eq!(unknown.body["error"]["code"], -32602);

    let get = raw_request(&address, "GET", "/plugin-ui/demo/rpc", "", b"");
    assert_eq!(get.status, 405);
    assert_eq!(raw_header_values(&get, "allow"), vec!["POST"]);

    // A stopped plugin is a tool error inside a successful JSON-RPC envelope.
    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let call = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &rpc_body(12, "status"),
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["id"], 12);
    assert_eq!(call.body["result"]["isError"], true);
    assert!(
        call.body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("not running"),
        "{}",
        call.body
    );
}

#[test]
fn plugin_web_ui_requires_a_session_when_lan_is_enabled() {
    let (sandbox, address) = web_ui_sandbox(43);

    let enabled = sandbox.run_svc(&["lan", "enable", "--bind", "0.0.0.0"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    let token = token_line(&enabled);
    let bearer = format!("Bearer {}", token);

    let body = rpc_body(21, "status");
    let page = raw_request(&address, "GET", "/plugin-ui/demo/", "", b"");
    assert_eq!(page.status, 401);
    let anonymous = raw_request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &body,
    );
    assert_eq!(anonymous.status, 401);
    // The 308 canonicalisation runs after auth, so a slashless URL is just
    // another unauthenticated /plugin-ui/* request when LAN is enabled.
    let canonical = raw_request(&address, "GET", "/plugin-ui/demo", "", b"");
    assert_eq!(canonical.status, 401);

    let started = request_with(
        &address,
        "POST",
        "/api/v1/services/demo/start",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(started.status, 200, "{}", started.body);

    let login = request_with(
        &address,
        "POST",
        "/login",
        "application/json",
        serde_json::to_string(&json!({"token": token}))
            .expect("login body")
            .as_bytes(),
        &[],
    );
    assert_eq!(login.status, 200, "{}", login.body);
    let session = cookie_value(&login, "svcd_session").expect("svcd_session cookie");
    let cookie = format!("svcd_session={}", session);

    let page = raw_request_with(
        &address,
        "GET",
        "/plugin-ui/demo/",
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));

    let origin = format!("http://{}", address);
    let call = request_with(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &body,
        &[("Cookie", cookie.as_str()), ("Origin", origin.as_str())],
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["id"], 21);
    assert_eq!(call.body["result"]["content"][0]["text"], "pong");

    // Cookie-authenticated /rpc still needs a same-origin Origin (CSRF guard).
    let no_origin = raw_request_with(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &body,
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(no_origin.status, 403);
    let foreign = raw_request_with(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        &body,
        &[
            ("Cookie", cookie.as_str()),
            ("Origin", "http://evil.example"),
        ],
    );
    assert_eq!(foreign.status, 403);
}

/// One installed plugin, exercised end to end across every surface this phase
/// built: HTTP install -> start -> MCP (Streamable HTTP and the stdio bridge
/// agree) -> config/reset -> Web UI -> logs -> stop shrinks the tool surface ->
/// uninstall removes it everywhere.
#[test]
fn phase_end_to_end_install_start_mcp_config_logs_and_uninstall() {
    let (sandbox, address) = web_ui_sandbox(34);
    let token = token_line(&sandbox.run_svc(&["lan", "token", "rotate"]));

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    assert_eq!(started.body["summary"]["state"], "running");

    // The CLI reports the same state the HTTP API just returned.
    let cli = sandbox.run_svc(&["status", "demo", "--json"]);
    assert_eq!(
        cli.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_status: Value = serde_json::from_slice(&cli.stdout).expect("svc status json");
    assert_eq!(cli_status["summary"]["state"], "running");

    // Streamable HTTP: a live plugin exposes start + its manifest capability.
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(listed.status, 200, "{}", listed.body);
    let http_tools = tool_names(&listed.body["result"]["tools"]);
    assert!(
        http_tools.contains(&"demo.status".to_string()),
        "{http_tools:?}"
    );
    assert!(
        http_tools.contains(&"demo.start".to_string()),
        "{http_tools:?}"
    );

    let called = mcp_request(
        &address,
        &token,
        "tools/call",
        json!({"name": "demo.status", "arguments": {}}),
        Some("demo.status"),
    );
    assert_eq!(called.status, 200, "{}", called.body);
    assert_eq!(called.body["result"]["isError"], false, "{}", called.body);
    assert_eq!(called.body["result"]["content"][0]["text"], "pong");

    // The stdio bridge must expose exactly the same surface and result.
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        http_tools,
        "{stdio_listed}"
    );
    let stdio_called = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "demo.status",
            "arguments": {},
            "_meta": modern_meta(),
        },
    }));
    assert_eq!(stdio_called["result"]["isError"], false, "{stdio_called}");
    assert_eq!(stdio_called["result"]["content"][0]["text"], "pong");

    // Config belongs to the same installed version.
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(config.status, 200);
    assert_eq!(config.body, b"initial config\n");
    let updated = raw_request(
        &address,
        "PUT",
        "/api/v1/config/demo",
        "text/plain",
        b"updated\n",
    );
    assert_eq!(
        updated.status,
        200,
        "{}",
        String::from_utf8_lossy(&updated.body)
    );
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(config.body, b"updated\n");
    let reset = raw_request(&address, "POST", "/api/v1/config/demo/reset", "", b"");
    assert_eq!(reset.status, 200);
    let config = raw_request(&address, "GET", "/api/v1/config/demo", "", b"");
    assert_eq!(config.body, b"initial config\n");

    // Web UI and logs are served from the same installation.
    let ui = raw_request(&address, "GET", "/plugin-ui/demo/", "", b"");
    assert_eq!(ui.status, 200);
    assert!(String::from_utf8_lossy(&ui.body).contains("<base href=\"/plugin-ui/demo/\">"));
    let logs = raw_request(&address, "GET", "/api/v1/logs/demo?lines=10", "", b"");
    assert_eq!(logs.status, 200, "{}", String::from_utf8_lossy(&logs.body));

    // A stopped plugin only exposes start, on both transports.
    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(
        tool_names(&listed.body["result"]["tools"]),
        vec!["demo.start"]
    );
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        vec!["demo.start"]
    );

    // Uninstall removes the service from every surface.
    let deleted = request(&address, "DELETE", "/api/v1/plugins/demo", "", b"");
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    let list = sandbox.run_svc(&["list"]);
    assert!(
        !String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("demo")),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(listed.status, 200);
    assert!(tool_names(&listed.body["result"]["tools"]).is_empty());
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert!(tool_names(&stdio_listed["result"]["tools"]).is_empty());
}

#[test]
fn plugin_ui_sse_streams_plugin_notifications() {
    let (_sandbox, address) = daemon_with_web_event_plugin(51);
    let mut sse = sse_connect(&address, "/plugin-ui/demo/events");
    let call = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(call.status, 200, "{}", call.body);
    assert_eq!(call.body["result"]["content"][0]["text"], "pong");
    let event = sse_wait_for_data(&mut sse, "pong-event");
    assert!(
        event.contains("\"method\":\"notifications/message\""),
        "{event}"
    );
    assert!(event.contains("id: "), "{event}");
}

#[test]
fn plugin_ui_websocket_bridges_calls_notifications_and_control_frames() {
    let (_sandbox, address) = daemon_with_web_event_plugin(52);
    let (mut ws, status, head) = ws_handshake(&address, "/plugin-ui/demo/ws", &[]);
    assert_eq!(status, 101, "{head}");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": { "name": "status", "arguments": {} }
    });
    ws_send(&mut ws, 0x1, request.to_string().as_bytes());

    let mut notification = false;
    let mut response = false;
    for _ in 0..6 {
        let (opcode, payload) = ws_read(&mut ws);
        if opcode != 0x1 {
            continue;
        }
        let value: Value = serde_json::from_slice(&payload).expect("ws json");
        if value["method"] == "notifications/message" {
            notification = true;
        }
        if value["id"] == 9 && value["result"]["content"][0]["text"] == "pong" {
            response = true;
        }
        if notification && response {
            break;
        }
    }
    assert!(notification, "notification frame missing");
    assert!(response, "response frame missing");

    ws_send(&mut ws, 0x9, b"ping");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0xA);
    assert_eq!(payload, b"ping");

    let (mut binary, status, head) = ws_handshake(&address, "/plugin-ui/demo/ws", &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut binary, 0x2, b"binary");
    let (opcode, payload) = ws_read(&mut binary);
    assert_eq!(opcode, 0x8);
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1003);
}

#[test]
fn plugin_ui_streams_require_lan_auth_and_same_origin() {
    let (sandbox, address) = daemon_with_web_event_plugin(53);
    let enabled = sandbox.run_svc(&["lan", "enable", "--bind", "0.0.0.0"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    let token = token_line(&enabled);
    let bearer = format!("Bearer {}", token);

    let sse = raw_request(&address, "GET", "/plugin-ui/demo/events", "", b"");
    assert_eq!(sse.status, 401);
    let (_stream, status, _head) = ws_handshake(&address, "/plugin-ui/demo/ws", &[]);
    assert_eq!(status, 401);

    let evil = ws_handshake(
        &address,
        "/plugin-ui/demo/ws",
        &[
            ("Authorization", bearer.as_str()),
            ("Origin", "http://evil"),
        ],
    );
    assert_eq!(evil.1, 403);

    let origin = format!("http://{}", address);
    let good = ws_handshake(
        &address,
        "/plugin-ui/demo/ws",
        &[
            ("Authorization", bearer.as_str()),
            ("Origin", origin.as_str()),
        ],
    );
    assert_eq!(good.1, 101, "{}", good.2);
}

#[test]
fn plugin_ui_sse_replays_after_last_event_id() {
    let (_sandbox, address) = daemon_with_web_event_plugin(54);
    let mut first = sse_connect(&address, "/plugin-ui/demo/events");
    let call = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        br#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(call.status, 200, "{}", call.body);
    let event = sse_wait_for_data(&mut first, "pong-event");
    let last_id = sse_id(&event).expect("event id");
    drop(first);

    // No SSE subscriber is attached now, so this event survives only in the
    // replay ring until the next connection asks for it.
    let second = request(
        &address,
        "POST",
        "/plugin-ui/demo/rpc",
        "application/json",
        br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(second.status, 200, "{}", second.body);

    let mut replay = sse_connect_with_last_id(&address, "/plugin-ui/demo/events", Some(last_id));
    let replayed = sse_wait_for_data(&mut replay, "pong-event");
    let replayed_id = sse_id(&replayed).expect("replayed event id");
    assert!(
        replayed_id > last_id,
        "expected an event after {last_id}, got {replayed_id}: {replayed}"
    );
}

#[test]
fn plugin_ui_websocket_rejects_oversized_messages_with_1009() {
    let (_sandbox, address) = daemon_with_web_event_plugin(55);
    let (mut ws, status, head) = ws_handshake(&address, "/plugin-ui/demo/ws", &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut ws, 0x1, &vec![b'x'; 1024 * 1024 + 1]);
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8);
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1009);
}

#[test]
fn http_keep_alive_serves_two_requests_on_one_connection() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");

    let first = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream.write_all(first.as_bytes()).expect("first request");
    let first = read_one_response(&mut stream);
    assert_eq!(first.status, 200);
    assert_eq!(raw_header_values(&first, "connection"), vec!["keep-alive"]);

    let second = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(second.as_bytes()).expect("second request");
    let second = read_one_response(&mut stream);
    assert_eq!(second.status, 200);
    assert_eq!(raw_header_values(&second, "connection"), vec!["close"]);
}
