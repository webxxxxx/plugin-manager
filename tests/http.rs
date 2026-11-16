use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
            .env("SVCD_BIND", "127.0.0.1:0")
            .env("SVCD_BOOT_SCAN_WAIT_MS", "0")
            .env("SVCD_RESCAN_INTERVAL_MS", "0")
            .env("SVCD_TEST_UNPRIVILEGED", "1");
    }

    /// Opt this sandbox's daemon into unsigned installs via `svcd.conf`.
    fn allow_insecure_install(&self) {
        self.allow_insecure_install_with("");
    }

    /// Same, plus extra `svcd.conf` keys for one test.
    fn allow_insecure_install_with(&self, extra: &str) {
        fs::write(
            self.config.join("svcd.conf"),
            format!("allow_insecure_install = true\n{}", extra),
        )
        .expect("write svcd.conf");
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
        self.start_daemon_with_bind("127.0.0.1:0");
    }

    fn start_daemon_with_bind(&mut self, bind: &str) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("daemon stderr");
        let mut command = Command::new(&self.svcd);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        self.env(&mut command);
        command.env("SVCD_BIND", bind);
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    /// Like `start_daemon`, but leaves the bind address to the config instead
    /// of forcing an ephemeral port through the environment.
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
            .env("SVCD_TEST_UNPRIVILEGED", "1")
            .env("SVCD_RESCAN_INTERVAL_MS", "0")
            .env("SVCD_BOOT_SCAN_WAIT_MS", "0");
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    fn start_daemon_with_conf(&mut self, conf: &Path) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("daemon stderr");
        let mut command = Command::new(&self.svcd);
        command
            .args([
                "run",
                "--foreground",
                "--conf",
                conf.to_str().expect("utf8 config path"),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TRUST_DIR", &self.trust)
            .env("SVCD_TEST_UNPRIVILEGED", "1")
            .env("SVCD_RESCAN_INTERVAL_MS", "0")
            .env("SVCD_BOOT_SCAN_WAIT_MS", "0");
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

/// Send one request with `Transfer-Encoding: chunked`, deliberately split into
/// small chunks so the decoder sees chunk boundaries in different reads.
fn raw_request_chunked(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> RawResponse {
    let mut stream = TcpStream::connect(address).expect("connect");
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n",
        method, path, address
    );
    if !content_type.is_empty() {
        head.push_str(&format!("Content-Type: {}\r\n", content_type));
    }
    for (name, value) in extra_headers {
        head.push_str(&format!("{}: {}\r\n", name, value));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).expect("write head");
    for chunk in body.chunks(7) {
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .expect("chunk size");
        stream.write_all(chunk).expect("chunk body");
        stream.write_all(b"\r\n").expect("chunk terminator");
    }
    stream.write_all(b"0\r\n\r\n").expect("last chunk");
    read_raw_response(&mut stream)
}

fn multipart_body(boundary: &str, filename: &str, content: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            boundary, filename
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());
    body
}

fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") else {
            break;
        };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.trim(), 16).unwrap_or(0);
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        out.extend_from_slice(&rest[..size.min(rest.len())]);
        rest = &rest[(size + 2).min(rest.len())..];
    }
    out
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
    let (manifest, files) = plugin_parts(id, version, config, check);
    signed_package(key, manifest, &files)
}

/// Same payload as `plugin_bundle`, but without the `signature.json` entry.
fn unsigned_plugin_bundle(id: &str) -> Vec<u8> {
    let (manifest, files) = plugin_parts(id, "1.0.0", None, false);
    unsigned_package(manifest, &files)
}

fn plugin_parts(
    id: &str,
    version: &str,
    config: Option<&[u8]>,
    check: bool,
) -> (Value, Vec<(String, Vec<u8>, u32)>) {
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
    (manifest, files)
}

fn attach_files(mut manifest: Value, files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    manifest["files"] = json!(files
        .iter()
        .map(|(path, data, mode)| json!({
            "path": path,
            "size": data.len(),
            "sha256": hex_sha256(data),
            "mode": mode
        }))
        .collect::<Vec<Value>>());
    serde_json::to_vec(&manifest).expect("manifest")
}

/// Attach \`files[]\`, sign the manifest and wrap everything in the ustar layout
/// the installer verifies. Shared by the plain and web-UI fixtures.
fn signed_package(key: &SigningKey, manifest: Value, files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    let manifest = attach_files(manifest, files);
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

/// A package with no `signature.json` entry at all; only the insecure install
/// path accepts it.
fn unsigned_package(manifest: Value, files: &[(String, Vec<u8>, u32)]) -> Vec<u8> {
    let manifest = attach_files(manifest, files);
    let mut tar = Vec::new();
    tar.extend_from_slice(&header("manifest.json", manifest.len(), 0o644));
    tar.extend_from_slice(&manifest);
    tar.extend(std::iter::repeat_n(0, padding(manifest.len())));
    for (path, data, mode) in files {
        tar.extend_from_slice(&header(path, data.len(), *mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

/// A `.spkb` backup wrapping an unsigned inner package.
fn unsigned_backup(id: &str) -> Vec<u8> {
    let spk = unsigned_plugin_bundle(id);
    let backup =
        serde_json::to_vec(&json!({"schema": 1, "id": id, "version": "1.0.0"})).expect("backup");
    let mut tar = Vec::new();
    for (path, data, mode) in [
        ("backup.json", backup.as_slice(), 0o644u32),
        ("plugin.spk", spk.as_slice(), 0o644u32),
    ] {
        tar.extend_from_slice(&header(path, data.len(), mode));
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
  $token = $null
  if ($line -match '"_meta"\s*:\s*\{[^}]*"progressToken"\s*:\s*"([^"]+)"') { $token = $Matches[1] }
  if ($null -ne $token) {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"' + $token + '"}}')
  }
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
  meta=$(printf '%s\n' "$line" | sed -n 's/.*"_meta":{\([^}]*\)}.*/\1/p')
  token=$(printf '%s\n' "$meta" | sed -n 's/.*"progressToken":"\([^"]*\)".*/\1/p')
  if [ -n "$token" ]; then
    printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"%s"}}\n' "$token"
  fi
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
    ws_send_with(stream, opcode, payload, true);
}

fn ws_send_with(stream: &mut TcpStream, opcode: u8, payload: &[u8], fin: bool) {
    let mask = [1u8, 2, 3, 4];
    let mut frame = vec![if fin { 0x80 | opcode } else { opcode }];
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

/// A protocol-3 (raw) plugin whose agent echoes stdin to stdout verbatim.
/// `cat` is enough: the daemon writes `len | stream | payload` frames and the
/// plugin must return them unchanged.
#[cfg(unix)]
fn raw_plugin_bundle(key: &SigningKey, id: &str) -> Vec<u8> {
    let exec_path = "payload/bin/raw-agent.sh".to_string();
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), b"#!/bin/sh\nexec cat\n".to_vec(), 0o755),
        (
            "payload/web/index.html".to_string(),
            b"<!doctype html>\n<html><body>raw</body></html>".to_vec(),
            0o644,
        ),
    ];
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let manifest = json!({
        "schema": 1,
        "name": id,
        "version": "1.0.0",
        "desc": "raw echo plugin",
        "platform": target(),
        "protocol": 3,
        "exec": {"path": exec_path, "args": [], "ready": "exec"},
        "ui": {"type": "web", "entry": "index.html"},
    });
    signed_package(key, manifest, &files)
}

/// A running daemon with a protocol-3 plugin named `echo`.
#[cfg(unix)]
fn daemon_with_raw_plugin(key_byte: u8) -> (Sandbox, String) {
    daemon_with_raw_plugin_with(key_byte, "")
}

#[cfg(unix)]
fn daemon_with_raw_plugin_with(key_byte: u8, conf: &str) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with(conf);
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = raw_plugin_bundle(&key, "echo");
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
    let started = request(&address, "POST", "/api/v1/services/echo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);
    (sandbox, address)
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
fn compat_endpoint_reports_the_host_facts() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let response = request(&address, "GET", "/api/v1/compat", "", b"");
    assert_eq!(response.status, 200);
    assert!(response.body["platform"].is_string(), "{}", response.body);
    assert!(response.body["device"].is_null(), "{}", response.body);
    assert!(
        response.body["marker_path"].is_string(),
        "{}",
        response.body
    );

    fs::write(sandbox.config.join("compat.conf"), "device = sr6110\n").expect("marker");
    let response = request(&address, "GET", "/api/v1/compat", "", b"");
    assert_eq!(response.status, 200);
    assert_eq!(response.body["device"], "sr6110", "{}", response.body);
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
    assert!(!ids.contains(&"svc"), "{ids:?}");
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
fn blobs_upload_and_download_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        b"blob body",
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let meta = json_body(&put);
    let id = meta["id"].as_str().expect("blob id").to_string();
    assert_eq!(meta["size"], 9);

    let get = raw_request(&address, "GET", &format!("/api/v1/blobs/{id}"), "", b"");
    assert_eq!(get.status, 200);
    assert_eq!(get.body, b"blob body");

    let missing = raw_request(&address, "GET", "/api/v1/blobs/sha256:deadbeef", "", b"");
    assert_eq!(missing.status, 404);
}

#[test]
fn blob_upload_accepts_chunked_and_multipart_bodies() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let body = b"chunked blob body";
    let chunked = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        body,
        &[],
    );
    assert_eq!(chunked.status, 200, "{:?}", chunked.body);
    let meta = json_body(&chunked);
    let expected = format!("sha256:{}", hex_sha256(body));
    assert_eq!(meta["id"], expected);
    assert_eq!(meta["size"], body.len());
    assert_eq!(
        raw_header_values(&chunked, "etag"),
        vec![format!("\"{}\"", expected).as_str()]
    );

    let boundary = "svcd-test-boundary";
    let multipart = multipart_body(boundary, "payload.bin", b"multipart blob body");
    let response = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        &format!("multipart/form-data; boundary={}", boundary),
        &multipart,
    );
    assert_eq!(response.status, 200, "{:?}", response.body);
    let meta = json_body(&response);
    let id = meta["id"].as_str().expect("blob id");
    let download = raw_request(&address, "GET", &format!("/api/v1/blobs/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, b"multipart blob body");

    // The same multipart body also works when the outer request is chunked.
    let chunked_multipart = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/blobs",
        &format!("multipart/form-data; boundary={}", boundary),
        &multipart,
        &[],
    );
    assert_eq!(
        chunked_multipart.status, 200,
        "{:?}",
        chunked_multipart.body
    );
    let id = json_body(&chunked_multipart)["id"]
        .as_str()
        .expect("blob id")
        .to_string();
    assert_eq!(id, meta["id"].as_str().expect("blob id"));
}

#[test]
fn file_bundle_upload_download_and_cross_part_range() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    assert_eq!(created.status, 200, "{}", created.body);
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();

    let first = request(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/1", upload_id),
        "application/octet-stream",
        b"hello ",
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let first_etag = first.body["etag"].as_str().expect("part etag").to_string();

    let second = raw_request_chunked(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"world",
        &[],
    );
    assert_eq!(second.status, 200, "{:?}", second.body);
    let second_etag = json_body(&second)["etag"]
        .as_str()
        .expect("part etag")
        .to_string();

    let third = request(
        &address,
        "PUT",
        &format!("/api/v1/files/uploads/{}/parts/3", upload_id),
        "application/octet-stream",
        b"!",
    );
    assert_eq!(third.status, 200, "{}", third.body);
    let third_etag = third.body["etag"].as_str().expect("part etag").to_string();

    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", upload_id),
        "application/json",
        serde_json::to_string(&json!({
            "name": "greeting.txt",
            "mimeType": "text/plain",
            "parts": [
                {"partNumber": 1, "etag": first_etag},
                {"partNumber": 2, "etag": second_etag},
                {"partNumber": 3, "etag": third_etag},
            ]
        }))
        .expect("complete body")
        .as_bytes(),
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("file id").to_string();
    assert_eq!(complete.body["size"], 12);
    assert_eq!(id, format!("sha256:{}", hex_sha256(b"hello world!")));

    let download = raw_request(&address, "GET", &format!("/api/v1/file/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, b"hello world!");
    assert_eq!(
        raw_header_values(&download, "content-type"),
        vec!["text/plain"]
    );
    assert_eq!(
        raw_header_values(&download, "etag"),
        vec![format!("\"{}\"", id).as_str()]
    );
    assert_eq!(
        raw_header_values(&download, "content-disposition"),
        vec!["attachment; filename=\"greeting.txt\""]
    );

    let range = raw_request_with(
        &address,
        "GET",
        &format!("/api/v1/files/{}", id),
        "",
        b"",
        &[("Range", "bytes=4-8")],
    );
    assert_eq!(range.status, 206);
    assert_eq!(range.body, b"o wor");
    assert_eq!(
        raw_header_values(&range, "content-range"),
        vec!["bytes 4-8/12"]
    );

    let head = raw_request(&address, "HEAD", &format!("/api/v1/file/{}", id), "", b"");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(raw_header_values(&head, "content-length"), vec!["12"]);

    // A second session with the same content reuses the content-addressed file.
    let created = request(&address, "POST", "/api/v1/files/uploads", "", b"");
    let duplicate_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    for (number, content) in [
        (1, b"hello ".as_slice()),
        (2, b"world".as_slice()),
        (3, b"!".as_slice()),
    ] {
        let part = request(
            &address,
            "PUT",
            &format!("/api/v1/files/uploads/{}/parts/{}", duplicate_id, number),
            "application/octet-stream",
            content,
        );
        assert_eq!(part.status, 200, "{}", part.body);
    }
    let duplicate = request(
        &address,
        "POST",
        &format!("/api/v1/files/uploads/{}/complete", duplicate_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1},{\"partNumber\":2},{\"partNumber\":3}]}",
    );
    assert_eq!(duplicate.status, 200, "{}", duplicate.body);
    assert_eq!(duplicate.body["id"].as_str(), Some(id.as_str()));
}

#[test]
fn blob_download_supports_range_etag_and_conditionals() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        b"0123456789",
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let id = json_body(&put)["id"].as_str().expect("blob id").to_string();
    let path = format!("/api/v1/blobs/{}", id);

    let full = raw_request(&address, "GET", &path, "", b"");
    assert_eq!(full.status, 200);
    assert_eq!(full.body, b"0123456789");
    let etag = raw_header_values(&full, "etag")
        .into_iter()
        .next()
        .expect("etag")
        .to_string();
    assert_eq!(etag, format!("\"{}\"", id));
    assert_eq!(raw_header_values(&full, "accept-ranges"), vec!["bytes"]);
    assert!(!raw_header_values(&full, "last-modified").is_empty());

    let range = raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=2-5")]);
    assert_eq!(range.status, 206);
    assert_eq!(range.body, b"2345");
    assert_eq!(
        raw_header_values(&range, "content-range"),
        vec!["bytes 2-5/10"]
    );

    let suffix = raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=-3")]);
    assert_eq!(suffix.status, 206);
    assert_eq!(suffix.body, b"789");

    let open = raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=7-")]);
    assert_eq!(open.status, 206);
    assert_eq!(open.body, b"789");

    let unsatisfiable =
        raw_request_with(&address, "GET", &path, "", b"", &[("Range", "bytes=99-")]);
    assert_eq!(unsatisfiable.status, 416);
    assert_eq!(
        raw_header_values(&unsatisfiable, "content-range"),
        vec!["bytes */10"]
    );

    let not_modified =
        raw_request_with(&address, "GET", &path, "", b"", &[("If-None-Match", &etag)]);
    assert_eq!(not_modified.status, 304);
    assert!(not_modified.body.is_empty());

    let future = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("If-Modified-Since", "Sun, 15 Nov 2099 00:00:00 GMT")],
    );
    assert_eq!(future.status, 304);

    let unmodified = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:00 GMT")],
    );
    assert_eq!(unmodified.status, 412);

    let failed = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("If-Match", "\"sha256:deadbeef\"")],
    );
    assert_eq!(failed.status, 412);

    let stale_range = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("Range", "bytes=2-5"), ("If-Range", "\"sha256:deadbeef\"")],
    );
    assert_eq!(stale_range.status, 200);
    assert_eq!(stale_range.body, b"0123456789");

    let fresh_range = raw_request_with(
        &address,
        "GET",
        &path,
        "",
        b"",
        &[("Range", "bytes=2-5"), ("If-Range", &etag)],
    );
    assert_eq!(fresh_range.status, 206);
    assert_eq!(fresh_range.body, b"2345");

    let head = raw_request(&address, "HEAD", &path, "", b"");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(raw_header_values(&head, "content-length"), vec!["10"]);

    let head_unsatisfiable =
        raw_request_with(&address, "HEAD", &path, "", b"", &[("Range", "bytes=99-")]);
    assert_eq!(head_unsatisfiable.status, 416);
    assert!(head_unsatisfiable.body.is_empty());

    let missing = raw_request(&address, "HEAD", "/api/v1/blobs/sha256:deadbeef", "", b"");
    assert_eq!(missing.status, 404);
    assert!(missing.body.is_empty());
}

#[test]
fn file_part_2mib_upload_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let content = vec![0x5a; 2 * 1024 * 1024];
    let part = raw_request(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/1", upload_id),
        "application/octet-stream",
        &content,
    );
    assert_eq!(part.status, 200, "{:?}", part.body);
    let etag = json_body(&part)["etag"]
        .as_str()
        .expect("part etag")
        .to_string();
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", upload_id),
        "application/json",
        serde_json::to_string(&json!({"parts": [{"partNumber": 1, "etag": etag}]}))
            .expect("complete body")
            .as_bytes(),
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("file id");
    let expected = format!("sha256:{}", hex_sha256(&content));
    assert_eq!(id, expected);
    let download = raw_request(&address, "GET", &format!("/api/v1/file/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body.len(), content.len());
    assert_eq!(download.body, content);
}

#[test]
fn blob_2mib_upload_and_multipart_complete_round_trip() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let direct = vec![0x33; 2 * 1024 * 1024];
    let put = raw_request(
        &address,
        "POST",
        "/api/v1/blobs",
        "application/octet-stream",
        &direct,
    );
    assert_eq!(put.status, 200, "{:?}", put.body);
    let direct_id = format!("sha256:{}", hex_sha256(&direct));
    assert_eq!(json_body(&put)["id"].as_str(), Some(direct_id.as_str()));
    let ranged = raw_request_with(
        &address,
        "GET",
        &format!("/api/v1/blobs/{}", direct_id),
        "",
        b"",
        &[("Range", "bytes=1048576-2097151")],
    );
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body, direct[1024 * 1024..2 * 1024 * 1024]);

    let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let first = vec![0x11; 2 * 1024 * 1024];
    let second = vec![0x22; 2 * 1024 * 1024];
    for (number, content) in [(1, first.as_slice()), (2, second.as_slice())] {
        let part = raw_request(
            &address,
            "PUT",
            &format!("/api/v1/blobs/uploads/{}/parts/{}", upload_id, number),
            "application/octet-stream",
            content,
        );
        assert_eq!(part.status, 200, "{:?}", part.body);
    }
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", upload_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1},{\"partNumber\":2}]}",
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let mut expected = first.clone();
    expected.extend_from_slice(&second);
    let expected_id = format!("sha256:{}", hex_sha256(&expected));
    assert_eq!(complete.body["id"].as_str(), Some(expected_id.as_str()));

    let downloaded = raw_request(
        &address,
        "GET",
        &format!("/api/v1/blobs/{}", expected_id),
        "",
        b"",
    );
    assert_eq!(downloaded.status, 200);
    assert_eq!(downloaded.body, expected);
}

#[test]
fn upload_sessions_reject_cross_kind_requests() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let file_created = request(&address, "POST", "/api/v1/file/uploads", "", b"");
    assert_eq!(file_created.status, 200);
    let file_id = file_created.body["uploadId"]
        .as_str()
        .expect("file upload id")
        .to_string();

    let blob_status = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", file_id),
        "",
        b"",
    );
    assert_eq!(blob_status.status, 409, "{:?}", blob_status.body);
    let blob_abort = request(
        &address,
        "DELETE",
        &format!("/api/v1/blobs/uploads/{}", file_id),
        "",
        b"",
    );
    assert_eq!(blob_abort.status, 409, "{:?}", blob_abort.body);
    let blob_complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", file_id),
        "application/json",
        b"",
    );
    assert_eq!(blob_complete.status, 409, "{:?}", blob_complete.body);
    let file_status = request(
        &address,
        "GET",
        &format!("/api/v1/file/uploads/{}", file_id),
        "",
        b"",
    );
    assert_eq!(file_status.status, 200);

    let file_part = raw_request(
        &address,
        "PUT",
        &format!("/api/v1/file/uploads/{}/parts/1", file_id),
        "application/octet-stream",
        b"file",
    );
    assert_eq!(file_part.status, 200);
    let file_complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", file_id),
        "application/json",
        b"",
    );
    assert_eq!(file_complete.status, 200, "{}", file_complete.body);

    let blob_created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    assert_eq!(blob_created.status, 200);
    let blob_id = blob_created.body["uploadId"]
        .as_str()
        .expect("blob upload id")
        .to_string();
    let file_status = request(
        &address,
        "GET",
        &format!("/api/v1/file/uploads/{}", blob_id),
        "",
        b"",
    );
    assert_eq!(file_status.status, 409, "{:?}", file_status.body);
    let file_abort = request(
        &address,
        "DELETE",
        &format!("/api/v1/file/uploads/{}", blob_id),
        "",
        b"",
    );
    assert_eq!(file_abort.status, 409, "{:?}", file_abort.body);
    let file_complete = request(
        &address,
        "POST",
        &format!("/api/v1/file/uploads/{}/complete", blob_id),
        "application/json",
        b"",
    );
    assert_eq!(file_complete.status, 409, "{:?}", file_complete.body);
    let blob_part = raw_request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/1", blob_id),
        "application/octet-stream",
        b"blob",
    );
    assert_eq!(blob_part.status, 200);
    let blob_complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", blob_id),
        "application/json",
        b"{\"parts\":[{\"partNumber\":1}]}",
    );
    assert_eq!(blob_complete.status, 200, "{}", blob_complete.body);
}

#[test]
fn chunked_bodies_work_for_management_json_routes() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let key = SigningKey::from_bytes(&[31u8; 32]);
    let pem = pem_from_public(&key.verifying_key().to_bytes());
    let chunked = raw_request_chunked(
        &address,
        "POST",
        "/api/v1/trust?key_id=chunked-key",
        "application/octet-stream",
        &pem,
        &[],
    );
    assert_eq!(chunked.status, 200, "{:?}", chunked.body);

    let listed = request(&address, "GET", "/api/v1/trust", "", b"");
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(listed.body["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .any(|key| key == "chunked-key"));
}

#[test]
fn blob_upload_honours_expect_100_continue() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let body = b"continue body";
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let head = format!(
        "POST /api/v1/blobs HTTP/1.1\r\nHost: {}\r\nContent-Type: application/octet-stream\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        body.len()
    );
    stream.write_all(head.as_bytes()).expect("write head");
    let mut interim = Vec::new();
    let mut byte = [0u8; 1];
    while !interim.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("100 continue");
        interim.push(byte[0]);
    }
    assert!(
        String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"),
        "{:?}",
        String::from_utf8_lossy(&interim)
    );
    stream.write_all(body).expect("write body");
    let response = read_raw_response(&mut stream);
    assert_eq!(response.status, 200, "{:?}", response.body);
}

#[test]
fn blob_resumable_upload_supports_parts_retries_and_parallel_uploads() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let created = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    assert_eq!(created.status, 200, "{}", created.body);
    let upload_id = created.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();

    let first = raw_request_chunked(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/1", upload_id),
        "application/octet-stream",
        b"hello ",
        &[],
    );
    assert_eq!(first.status, 200, "{:?}", first.body);
    let first_etag = json_body(&first)["etag"]
        .as_str()
        .expect("part etag")
        .to_string();

    let second = request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"world",
    );
    assert_eq!(second.status, 200, "{}", second.body);
    let second_etag = second.body["etag"].as_str().expect("part etag").to_string();

    let status = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", upload_id),
        "",
        b"",
    );
    assert_eq!(status.status, 200, "{}", status.body);
    assert_eq!(status.body["totalSize"], 11);
    assert_eq!(status.body["parts"].as_array().expect("parts").len(), 2);

    // Re-uploading the same part number replaces it.
    let replaced = request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"there",
    );
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    let replaced_etag = replaced.body["etag"]
        .as_str()
        .expect("part etag")
        .to_string();
    let status = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", upload_id),
        "",
        b"",
    );
    assert_eq!(status.body["totalSize"], 11);

    // Replace it back so the assembled content is predictable.
    let restored = request(
        &address,
        "PUT",
        &format!("/api/v1/blobs/uploads/{}/parts/2", upload_id),
        "application/octet-stream",
        b"world",
    );
    assert_eq!(restored.status, 200, "{}", restored.body);
    let restored_etag = restored.body["etag"]
        .as_str()
        .expect("part etag")
        .to_string();
    assert_eq!(restored_etag, second_etag);
    assert_ne!(replaced_etag, second_etag);

    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", upload_id),
        "application/json",
        serde_json::to_string(&json!({
            "parts": [
                {"partNumber": 1, "etag": first_etag},
                {"partNumber": 2, "etag": restored_etag},
            ]
        }))
        .expect("complete body")
        .as_bytes(),
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("blob id");
    let expected = format!("sha256:{}", hex_sha256(b"hello world"));
    assert_eq!(id, expected);
    let download = raw_request(&address, "GET", &format!("/api/v1/blobs/{}", id), "", b"");
    assert_eq!(download.status, 200);
    assert_eq!(download.body, b"hello world");

    let after = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", upload_id),
        "",
        b"",
    );
    assert_eq!(after.status, 404, "{}", after.body);

    // Two independent upload sessions can write parts concurrently.
    let parallel = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    let parallel_id = parallel.body["uploadId"]
        .as_str()
        .expect("upload id")
        .to_string();
    let left = {
        let address = address.clone();
        let id = parallel_id.clone();
        std::thread::spawn(move || {
            raw_request(
                &address,
                "PUT",
                &format!("/api/v1/blobs/uploads/{}/parts/1", id),
                "application/octet-stream",
                b"left-",
            )
        })
    };
    let right = {
        let address = address.clone();
        let id = parallel_id.clone();
        std::thread::spawn(move || {
            raw_request(
                &address,
                "PUT",
                &format!("/api/v1/blobs/uploads/{}/parts/2", id),
                "application/octet-stream",
                b"right",
            )
        })
    };
    assert_eq!(left.join().expect("left").status, 200);
    assert_eq!(right.join().expect("right").status, 200);
    let complete = request(
        &address,
        "POST",
        &format!("/api/v1/blobs/uploads/{}/complete", parallel_id),
        "",
        b"",
    );
    assert_eq!(complete.status, 200, "{}", complete.body);
    let id = complete.body["id"].as_str().expect("blob id");
    let download = raw_request(&address, "GET", &format!("/api/v1/blobs/{}", id), "", b"");
    assert_eq!(download.body, b"left-right");

    let aborted = request(&address, "POST", "/api/v1/blobs/uploads", "", b"");
    let abort_id = aborted.body["uploadId"].as_str().expect("upload id");
    let deleted = request(
        &address,
        "DELETE",
        &format!("/api/v1/blobs/uploads/{}", abort_id),
        "",
        b"",
    );
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    let gone = request(
        &address,
        "GET",
        &format!("/api/v1/blobs/uploads/{}", abort_id),
        "",
        b"",
    );
    assert_eq!(gone.status, 404);
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
fn purge_delete_removes_the_plugin_tree_and_logs() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[24u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle(&key, "demo", "1.0.0", None, false);
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

    let log = sandbox.logs.join("demo.log");
    assert!(log.is_file(), "the service log should exist after start");

    let deleted = request(
        &address,
        "DELETE",
        "/api/v1/plugins/demo?purge=true",
        "",
        b"",
    );
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    assert!(!plugin_dir(&sandbox, "demo").exists());
    for _ in 0..40 {
        if !log.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!log.exists(), "purge must remove the service log");
}

#[test]
fn trust_upload_enables_a_signed_install() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[41u8; 32]);
    let package = plugin_package(&key);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    // Without a trust root the upload is rejected with an actionable error.
    let untrusted = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(untrusted.status, 400, "{}", untrusted.body);
    assert!(
        untrusted.body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("svc trust add"),
        "{}",
        untrusted.body
    );

    let pem = pem_from_public(&key.verifying_key().to_bytes());
    let added = request(
        &address,
        "POST",
        "/api/v1/trust?key_id=test",
        "application/octet-stream",
        &pem,
    );
    assert_eq!(added.status, 200, "{}", added.body);
    assert_eq!(added.body["keyId"], "test");
    assert!(sandbox.trust.join("test.pub").is_file());

    let listed = request(&address, "GET", "/api/v1/trust", "", b"");
    assert_eq!(listed.status, 200);
    assert!(listed.body["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .any(|key| key == "test"));

    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);

    let removed = request(&address, "DELETE", "/api/v1/trust/test", "", b"");
    assert_eq!(removed.status, 200, "{}", removed.body);
    assert!(!sandbox.trust.join("test.pub").exists());
}

#[test]
fn explicit_conf_is_preferred_over_program_and_config() {
    fn write_core(dir: &Path, id: &str) {
        fs::create_dir_all(dir).expect("core dir");
        let (exec, args) = if cfg!(windows) {
            (
                r"C:\Windows\System32\cmd.exe".to_string(),
                json!(["/c", "exit", "0"]),
            )
        } else {
            ("/bin/true".to_string(), json!([]))
        };
        fs::write(
            dir.join(format!("{}.json", id)),
            serde_json::to_vec(&json!({
                "schema": 1,
                "id": id,
                "version": "1.0.0",
                "desc": "conf precedence service",
                "protocol": 1,
                "exec": {"path": exec, "args": args, "ready": "exec"}
            }))
            .expect("core definition"),
        )
        .expect("write core definition");
    }

    let mut sandbox = Sandbox::new();
    let explicit_services = sandbox.root.join("explicit-services");
    let decoy_services = sandbox.root.join("decoy-services");
    write_core(&explicit_services, "explicit");
    write_core(&decoy_services, "decoy");

    let explicit = sandbox.root.join("override.conf");
    fs::write(
        &explicit,
        format!(
            "bind = 127.0.0.1:0\nplugins_dir = {}\nservices_dir = {}\n",
            sandbox.root.join("explicit-plugins").display(),
            explicit_services.display()
        ),
    )
    .expect("write explicit conf");
    fs::write(
        sandbox.root.join("svcd.conf"),
        format!(
            "bind = 127.0.0.1:0\nplugins_dir = {}\nservices_dir = {}\n",
            sandbox.root.join("decoy-plugins").display(),
            decoy_services.display()
        ),
    )
    .expect("write program-dir decoy conf");

    sandbox.start_daemon_with_conf(&explicit);
    let address = sandbox.wait_for_http();
    let services_response = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(services_response.status, 200);
    let ids: Vec<&str> = services_response.body["services"]
        .as_array()
        .expect("services")
        .iter()
        .filter_map(|service| service["id"].as_str())
        .collect();
    assert!(ids.contains(&"explicit"), "{ids:?}");
    assert!(
        !ids.contains(&"decoy"),
        "explicit --conf must win over the program-dir config: {ids:?}"
    );
}

#[test]
fn default_conf_is_created_in_the_config_dir() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_http();

    let path = sandbox.config.join("svcd.conf");
    let text = fs::read_to_string(&path).expect("generated svcd.conf");
    assert!(text.contains("bind = 127.0.0.1:8787"), "{text}");
    assert!(text.contains("allow_insecure_install = false"), "{text}");
    assert!(
        text.contains(&format!("config_dir = {}", sandbox.config.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "plugins_dir = {}",
            sandbox.data.join("plugins").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "services_dir = {}",
            sandbox.config.join("services.d").display()
        )),
        "{text}"
    );
}

#[test]
fn default_install_strategy_is_replace() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[25u8; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let v1 = plugin_bundle(&key, "demo", "1.0.0", None, false);
    let v2 = plugin_bundle(&key, "demo", "1.1.0", None, false);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let first = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &v1,
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let plugin = sandbox.data.join("plugins").join("demo");
    assert!(plugin.join("1.0.0").is_dir());

    let second = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &v2,
    );
    assert_eq!(second.status, 200, "{}", second.body);
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.1.0"
    );
    assert!(plugin.join("1.1.0").is_dir());
    assert!(
        !plugin.join("1.0.0").exists(),
        "the default API install must replace the old version"
    );
}

#[test]
fn untrusted_upload_returns_bad_request_not_404() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[22u8; 32]);
    // Deliberately leave the trust directory empty: the package is signed by
    // `test`, but the installer has no `trust.d/test.pub`.
    let package = plugin_package(&key);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let response = request(
        &address,
        "POST",
        "/api/v1/plugins?strategy=replace",
        "application/octet-stream",
        &package,
    );
    assert_eq!(response.status, 400, "{}", response.body);
    assert_eq!(response.body["error"]["code"], "bad_request");
    let message = response.body["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("unknown keyId test"), "{message}");
    assert!(message.contains("trust.d"), "{message}");
}

#[test]
fn unsigned_install_requires_the_conf_switch() {
    let mut sandbox = Sandbox::new();
    let package = unsigned_plugin_bundle("demo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let rejected = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &package,
    );
    assert_eq!(rejected.status, 400, "{}", rejected.body);
    let message = rejected.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("signature.json"), "{message}");
}

#[test]
fn unsigned_install_is_accepted_when_the_conf_switch_is_on() {
    let mut sandbox = Sandbox::new();
    let package = unsigned_plugin_bundle("demo");
    sandbox.allow_insecure_install();
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
    assert_eq!(installed.body["id"], "demo");
    assert_eq!(installed.body["version"], "1.0.0");
    let version_dir = sandbox.data.join("plugins").join("demo").join("1.0.0");
    assert!(version_dir.join("manifest.json").is_file());
    assert!(
        !version_dir.join("signature.json").exists(),
        "an insecure install must not invent a signature file"
    );

    // The version directory has no signature, so it cannot be exported as a
    // signed .spk; the error must say why instead of failing on a missing file.
    let exported = request(&address, "GET", "/api/v1/export/demo/spk", "", b"");
    assert_eq!(exported.status, 500, "{}", exported.body);
    let message = exported.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("no signature"), "{message}");
}

#[test]
fn insecure_install_accepts_an_unknown_signing_key() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[42u8; 32]);
    // Deliberately leave the trust directory empty: `test` is not a trust root.
    let package = plugin_package(&key);
    sandbox.allow_insecure_install();
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
    assert_eq!(installed.body["id"], "demo");
}

#[test]
fn unsigned_import_requires_the_conf_switch() {
    let mut sandbox = Sandbox::new();
    let backup = unsigned_backup("demo");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let rejected = request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(rejected.status, 400, "{}", rejected.body);
    let message = rejected.body["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("signature.json"), "{message}");
}

#[test]
fn unsigned_import_is_accepted_when_the_conf_switch_is_on() {
    let mut sandbox = Sandbox::new();
    let backup = unsigned_backup("demo");
    sandbox.allow_insecure_install();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let imported = request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &backup,
    );
    assert_eq!(imported.status, 200, "{}", imported.body);
    assert_eq!(imported.body["id"], "demo");
    assert_eq!(imported.body["version"], "1.0.0");
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
            "bind = 127.0.0.1:0\nplugins_dir = {}\nservices_dir = {}\n",
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
fn http_mcp_streams_progress_for_tools_call() {
    let (sandbox, address) = daemon_with_web_event_plugin(61);
    set_password(&sandbox, TEST_PASSWORD);

    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "demo.status",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "t1"
            }
        }
    }))
    .expect("body");

    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: demo.status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        TEST_PASSWORD,
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");

    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("headers");
    let head = String::from_utf8_lossy(&response[..split]);
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/event-stream"),
        "{head}"
    );
    let text = String::from_utf8_lossy(&decode_chunked(&response[split + 4..])).to_string();
    assert!(text.contains("notifications/progress"), "{text}");
    assert!(text.contains("\"progressToken\":\"t1\""), "{text}");
    assert!(text.contains("\"id\":7"), "{text}");
    assert!(text.contains("pong"), "{text}");
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

const TEST_PASSWORD: &str = "test-password";
const TEST_PASSWORD_2: &str = "new-test-password";

fn set_password(sandbox: &Sandbox, password: &str) {
    let changed = sandbox.run_svc(&["auth", "passwd", password]);
    assert_eq!(
        changed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
}

fn enable_auth(sandbox: &mut Sandbox) -> (String, String) {
    set_password(sandbox, TEST_PASSWORD);
    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();
    (address, TEST_PASSWORD.to_string())
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

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let summaries = services.body["services"].as_array().expect("services");
    let demo = summaries
        .iter()
        .find(|service| service["id"] == "demo")
        .expect("demo");
    let plain = summaries
        .iter()
        .find(|service| service["id"] == "plain")
        .expect("plain");
    assert_eq!(demo["hasConfig"], true);
    assert_eq!(plain["hasConfig"], false);

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

    let spk = raw_request(&address, "GET", "/api/v1/export/demo/spk", "", b"");
    assert_eq!(spk.status, 200, "{}", String::from_utf8_lossy(&spk.body));
    assert!(raw_header_values(&spk, "content-disposition")[0].contains("demo-1.0.0.spk"));
    let names = tar_names(&spk.body);
    assert_eq!(names.first().map(String::as_str), Some("manifest.json"));
    assert!(
        names.iter().any(|name| name == "signature.json"),
        "{names:?}"
    );

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

    // A backup taken now must contain the active config, not the template.
    let active_backup = raw_request(&address, "GET", "/api/v1/export/demo", "", b"");
    assert_eq!(active_backup.status, 200);
    let changed = raw_request(
        &address,
        "POST",
        "/api/v1/import?config_id=demo",
        "text/plain",
        b"changed\n",
    );
    assert_eq!(changed.status, 200);
    assert_eq!(fs::read(&active).expect("config"), b"changed\n");
    let restored = raw_request(
        &address,
        "POST",
        "/api/v1/import?force=true",
        "application/octet-stream",
        &active_backup.body,
    );
    assert_eq!(restored.status, 200);
    assert_eq!(json_body(&restored)["configApplied"], true);
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
        !html.contains("允许安装未签名包"),
        "unsigned install is controlled by svcd.conf, not a console checkbox"
    );
    assert!(
        html.contains("/api/v1/services"),
        "console does not call the management API"
    );
    for needle in [
        "上传安装",
        "上传公钥",
        "自动刷新（3s）",
        "hasConfig",
        "hasUI",
        "打开",
        "安装包",
        "导出备份",
        ".spkb",
        "purge=true",
        "/spk",
    ] {
        assert!(html.contains(needle), "console missing {}", needle);
    }
    let index = raw_request(&address, "GET", "/index.html", "", b"");
    assert_eq!(index.status, 200);

    let post = raw_request(&address, "POST", "/", "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET"]);
}

#[test]
fn lan_bind_without_password_falls_back_to_loopback() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();

    let open = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(open.status, 200);
    let status = sandbox.run_svc(&["auth", "status"]);
    assert_eq!(status.status.code(), Some(0));
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("requested: 0.0.0.0:0"), "{text}");
    assert!(text.contains("auth required: false"), "{text}");
}

#[test]
fn password_auth_bearer_cookie_csrf() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let open = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(open.status, 200);

    let before_password = sandbox.run_svc(&["auth", "enable", "--bind", "0.0.0.0"]);
    assert_ne!(before_password.status.code(), Some(0));

    let changed = sandbox.run_svc(&["auth", "passwd", TEST_PASSWORD]);
    assert_eq!(
        changed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    // auth is derived from bind, so restart with a LAN bind after the password
    // exists. No `auth enable` step is involved anymore.
    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();
    let password = TEST_PASSWORD.to_string();
    assert!(!sandbox.config.join("lan-token").exists());
    assert!(!sandbox.config.join("lan.json").exists());

    let page = raw_request(&address, "GET", "/", "", b"");
    assert_eq!(page.status, 200, "console must load before login");

    let denied = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(denied.status, 401);
    assert_eq!(denied.body["error"]["code"], "unauthorized");

    // `/api/v1/version` moved behind the S4 face; the server still resolves
    // authentication before the face runs, so it stays behind the same gate.
    let denied_version = request(&address, "GET", "/api/v1/version", "", b"");
    assert_eq!(denied_version.status, 401);
    assert_eq!(denied_version.body["error"]["code"], "unauthorized");

    let bearer = format!("Bearer {}", password);
    let allowed = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(allowed.status, 200, "{}", allowed.body);
    let allowed_version = request_with(
        &address,
        "GET",
        "/api/v1/version",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(allowed_version.status, 200, "{}", allowed_version.body);
    assert_eq!(allowed_version.body["protocol"], 1);

    let login = request_with(
        &address,
        "POST",
        "/login",
        "application/json",
        serde_json::to_string(&json!({ "password": password }))
            .expect("login")
            .as_bytes(),
        &[],
    );
    assert_eq!(login.status, 200, "{}", login.body);
    let cookie = cookie_value(&login, "svcd_api").expect("svcd_api cookie");
    let session = cookie_value(&login, "svcd_session").expect("svcd_session cookie");
    let cookies = header_values(&login, "set-cookie").join("\n");
    assert!(cookies.contains("Path=/api/v1/"), "{cookies}");
    let session_header = header_values(&login, "set-cookie")
        .into_iter()
        .find(|value| value.starts_with("svcd_session="))
        .expect("svcd_session header");
    // The session cookie must be sent to /logout, not only to /plugin-ui/.
    assert!(session_header.contains("Path=/;"), "{session_header}");
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

    let auth_status = request_with(
        &address,
        "GET",
        "/api/v1/auth",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(auth_status.status, 200);
    assert_eq!(auth_status.body["authRequired"], true);
    assert_eq!(auth_status.body["passwordSet"], true);
    assert!(
        auth_status.body["bind"]
            .as_str()
            .unwrap_or_default()
            .starts_with("0.0.0.0:"),
        "{}",
        auth_status.body
    );

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

    let changed = sandbox.run_svc(&["auth", "passwd", TEST_PASSWORD_2]);
    assert_eq!(changed.status.code(), Some(0));
    let old_bearer = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(old_bearer.status, 401);
    let new_bearer = format!("Bearer {}", TEST_PASSWORD_2);
    let new_allowed = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", new_bearer.as_str())],
    );
    assert_eq!(new_allowed.status, 200);

    let status = sandbox.run_svc(&["auth", "status"]);
    assert_eq!(status.status.code(), Some(0));
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(status_text.contains("auth required: true"), "{status_text}");
    assert!(status_text.contains("password: set"), "{status_text}");

    sandbox.shutdown();
    sandbox.start_daemon_with_bind("0.0.0.0:0");
    let address = sandbox.wait_for_http();
    let denied_after_restart = request(&address, "GET", "/api/v1/services", "", b"");
    assert_eq!(denied_after_restart.status, 401);
    let new_bearer = format!("Bearer {}", TEST_PASSWORD_2);
    let allowed_after_restart = request_with(
        &address,
        "GET",
        "/api/v1/services",
        "",
        b"",
        &[("Authorization", new_bearer.as_str())],
    );
    assert_eq!(allowed_after_restart.status, 200);
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

    let services = request(&address, "GET", "/api/v1/services", "", b"");
    let summaries = services.body["services"].as_array().expect("services");
    let demo = summaries
        .iter()
        .find(|service| service["id"] == "demo")
        .expect("demo");
    let plain = summaries
        .iter()
        .find(|service| service["id"] == "plain")
        .expect("plain");
    assert_eq!(demo["hasUI"], true);
    assert_eq!(plain["hasUI"], false);

    let base = plugin_ui_base(&address, "demo");

    let page = raw_request(&address, "GET", &base, "", b"");
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
        html.starts_with(&format!("<!doctype html>\n<base href=\"{}\">\n", base)),
        "{html}"
    );
    assert!(html.contains("plugin page"), "{html}");

    let etags = raw_header_values(&page, "etag");
    assert_eq!(etags.len(), 1, "{:?}", page.headers);
    let etag = etags[0].to_string();
    let revalidated = raw_request_with(
        &address,
        "GET",
        &base,
        "",
        b"",
        &[("If-None-Match", etag.as_str())],
    );
    assert_eq!(revalidated.status, 304);
    assert!(revalidated.body.is_empty());
    assert_eq!(raw_header_values(&revalidated, "etag"), vec![etag.as_str()]);

    let script = raw_request(&address, "GET", &format!("{}app.js", base), "", b"");
    assert_eq!(script.status, 200);
    assert_eq!(
        raw_header_values(&script, "content-type"),
        vec!["text/javascript"]
    );
    assert_eq!(script.body, b"export const answer = 42;\n");

    // The canonical URL has a trailing slash so relative assets resolve.
    let no_slash = raw_request(&address, "GET", base.trim_end_matches('/'), "", b"");
    assert_eq!(no_slash.status, 308);
    assert_eq!(
        raw_header_values(&no_slash, "location"),
        vec![base.as_str()]
    );

    // SPA fallback only for HTML navigations; a missing asset stays a 404.
    let missing_asset = raw_request(&address, "GET", &format!("{}missing.js", base), "", b"");
    assert_eq!(missing_asset.status, 404);
    let spa = raw_request_with(
        &address,
        "GET",
        &format!("{}settings/tunnel", base),
        "",
        b"",
        &[("Accept", "text/html")],
    );
    assert_eq!(spa.status, 200);
    assert!(String::from_utf8_lossy(&spa.body).contains("plugin page"));

    for path in ["/plugin-ui/Demo/", "/plugin-ui/missing/"] {
        let denied = raw_request(&address, "GET", path, "", b"");
        assert_eq!(denied.status, 404, "{path}");
        // A rejected path is not a client-side route: an HTML navigation must
        // not turn traversal into the SPA entry.
        let denied_html =
            raw_request_with(&address, "GET", path, "", b"", &[("Accept", "text/html")]);
        assert_eq!(denied_html.status, 404, "{path} with Accept: text/html");
    }

    // A missing token is refused even for an installed UI.
    let no_token = raw_request(&address, "GET", "/plugin-ui/demo/", "", b"");
    assert_eq!(no_token.status, 401);

    // A plugin without manifest.ui has no route even if payload/web exists.
    let no_ui = raw_request(&address, "GET", "/plugin-ui/plain/", "", b"");
    assert_eq!(no_ui.status, 404);

    let post = raw_request(&address, "POST", &base, "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET"]);
}

fn plugin_ui_base(address: &str, id: &str) -> String {
    plugin_ui_base_with(address, id, &[])
}

fn plugin_ui_base_with(address: &str, id: &str, headers: &[(&str, &str)]) -> String {
    let response = request_with(
        address,
        "POST",
        &format!("/api/v1/plugins/{}/ui-token", id),
        "",
        b"",
        headers,
    );
    assert_eq!(response.status, 200, "{}", response.body);
    response.body["url"]
        .as_str()
        .expect("plugin ui url")
        .to_string()
}

#[test]
fn plugin_ui_token_is_bound_to_its_plugin() {
    let (_sandbox, address) = web_ui_sandbox(44);
    let key = SigningKey::from_bytes(&[44u8; 32]);
    let other = web_plugin_bundle(&key, "other", true);
    let installed = request(
        &address,
        "POST",
        "/api/v1/plugins",
        "application/octet-stream",
        &other,
    );
    assert_eq!(installed.status, 200, "{}", installed.body);

    let demo_base = plugin_ui_base(&address, "demo");
    let own = plugin_ui_base(&address, "other");
    let page = raw_request(&address, "GET", &own, "", b"");
    assert_eq!(page.status, 200);

    // The demo token must not unlock the other plugin's UI.
    let cross = demo_base.replacen("/plugin-ui/demo/", "/plugin-ui/other/", 1);
    let denied = raw_request(&address, "GET", &cross, "", b"");
    assert_eq!(denied.status, 401);
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
    let base = plugin_ui_base(&address, "demo");

    let started = request(&address, "POST", "/api/v1/services/demo/start", "", b"");
    assert_eq!(started.status, 200, "{}", started.body);

    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
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
        &format!("{}rpc", base),
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
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Origin", origin.as_str())],
    );
    assert_eq!(same_origin.status, 200, "{}", same_origin.body);
    let cross_origin = request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Origin", "http://evil.example")],
    );
    assert_eq!(cross_origin.status, 403);

    let unsupported = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":10,"method":"svcd.ping"}"#,
    );
    assert_eq!(unsupported.status, 200);
    assert_eq!(unsupported.body["id"], 10);
    assert_eq!(unsupported.body["error"]["code"], -32601);

    let malformed = raw_request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        b"{",
    );
    assert_eq!(malformed.status, 200);
    assert_eq!(json_body(&malformed)["error"]["code"], -32700);

    let unknown = request(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &rpc_body(11, "nope"),
    );
    assert_eq!(unknown.body["id"], 11);
    assert_eq!(unknown.body["error"]["code"], -32602);

    let get = raw_request(&address, "GET", &format!("{}rpc", base), "", b"");
    assert_eq!(get.status, 405);
    assert_eq!(raw_header_values(&get, "allow"), vec!["POST"]);

    // A stopped plugin is a tool error inside a successful JSON-RPC envelope.
    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
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
    let (mut sandbox, _) = web_ui_sandbox(43);

    let (address, password) = enable_auth(&mut sandbox);
    let bearer = format!("Bearer {}", password);
    let base = plugin_ui_base_with(&address, "demo", &[("Authorization", bearer.as_str())]);

    let body = rpc_body(21, "status");
    let page = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(page.status, 401);
    let browsed = raw_request_with(&address, "GET", &base, "", b"", &[("Accept", "text/html")]);
    assert_eq!(browsed.status, 303);
    assert!(raw_header_values(&browsed, "location")[0].contains(&format!("/?next={}", base)));
    let anonymous = raw_request(
        &address,
        "POST",
        &format!("{}rpc", base),
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
        serde_json::to_string(&json!({"password": password}))
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
        &base,
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));

    let origin = format!("http://{}", address);
    let call = request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
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
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(no_origin.status, 403);
    let foreign = raw_request_with(
        &address,
        "POST",
        &format!("{}rpc", base),
        "application/json",
        &body,
        &[
            ("Cookie", cookie.as_str()),
            ("Origin", "http://evil.example"),
        ],
    );
    assert_eq!(foreign.status, 403);

    // Logout must revoke the session server-side, not just clear the console's
    // local token: a plugin tab that keeps the old cookie must not stay logged in.
    let logout = request_with(
        &address,
        "POST",
        "/logout",
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(logout.status, 200);
    let after_logout = raw_request_with(
        &address,
        "GET",
        &base,
        "",
        b"",
        &[("Cookie", cookie.as_str())],
    );
    assert_eq!(after_logout.status, 401);
}

/// One installed plugin, exercised end to end across every surface this phase
/// built: HTTP install -> start -> MCP (Streamable HTTP and the stdio bridge
/// agree) -> config/reset -> Web UI -> logs -> stop shrinks the tool surface ->
/// uninstall removes it everywhere.
#[test]
fn phase_end_to_end_install_start_mcp_config_logs_and_uninstall() {
    let (sandbox, address) = web_ui_sandbox(34);
    set_password(&sandbox, TEST_PASSWORD);
    let token = TEST_PASSWORD.to_string();
    let base = plugin_ui_base(&address, "demo");

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
    let ui = raw_request(&address, "GET", &base, "", b"");
    assert_eq!(ui.status, 200);
    assert!(String::from_utf8_lossy(&ui.body).contains(&format!("<base href=\"{}\">", base)));
    let logs = raw_request(&address, "GET", "/api/v1/logs/demo?lines=10", "", b"");
    assert_eq!(logs.status, 200, "{}", String::from_utf8_lossy(&logs.body));

    // A stopped plugin only exposes start, on both transports.
    let stopped = request(&address, "POST", "/api/v1/services/demo/stop", "", b"");
    assert_eq!(stopped.status, 200, "{}", stopped.body);
    let listed = mcp_request(&address, &token, "tools/list", json!({}), None);
    assert_eq!(
        tool_names(&listed.body["result"]["tools"]),
        vec!["demo.start", "svc.health", "svc.version"]
    );
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        vec!["demo.start", "svc.health", "svc.version"]
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
    assert_eq!(
        tool_names(&listed.body["result"]["tools"]),
        vec!["svc.health", "svc.version"]
    );
    let stdio_listed = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/list",
        "params": {"_meta": modern_meta()},
    }));
    assert_eq!(
        tool_names(&stdio_listed["result"]["tools"]),
        vec!["svc.health", "svc.version"]
    );
}

#[test]
fn plugin_ui_sse_streams_plugin_notifications() {
    let (_sandbox, address) = daemon_with_web_event_plugin(51);
    let base = plugin_ui_base(&address, "demo");
    let mut sse = sse_connect(&address, &format!("{}events", base));
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
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
    let base = plugin_ui_base(&address, "demo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
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

    let (mut binary, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut binary, 0x2, b"binary");
    let (opcode, payload) = ws_read(&mut binary);
    assert_eq!(opcode, 0x2);
    assert_eq!(payload, b"binary");
}

/// The `/raw` route only exists for protocol-3 plugins: a JSON-RPC plugin has
/// no raw transport, and a platform without a plugin-stdio reactor says so
/// instead of returning a misleading 404.
#[test]
fn plugin_ui_raw_requires_a_raw_capable_plugin() {
    let (_sandbox, address) = web_ui_sandbox(63);
    let base = plugin_ui_base(&address, "demo");
    let (mut stream, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    let mut body = Vec::new();
    let _ = stream.read_to_end(&mut body);
    let body = String::from_utf8_lossy(&body).to_ascii_lowercase();
    if cfg!(windows) {
        assert_eq!(status, 501, "{head}");
        assert!(body.contains("not_supported"), "{head}{body}");
    } else {
        assert_eq!(status, 503, "{head}");
        assert!(body.contains("raw_unavailable"), "{head}{body}");
    }
}

#[cfg(unix)]
#[test]
fn plugin_ui_raw_round_trips_binary_frames() {
    let (_sandbox, address) = daemon_with_raw_plugin(61);
    let base = plugin_ui_base(&address, "echo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");

    // One binary frame crosses the plugin link byte for byte.
    ws_send(&mut ws, 0x2, b"raw-bytes\x00\xff");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(payload, b"raw-bytes\x00\xff");

    // A fragmented message is reassembled before it reaches the plugin.
    ws_send_with(&mut ws, 0x2, b"part-one/", false);
    ws_send_with(&mut ws, 0x0, b"part-two", true);
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(payload, b"part-one/part-two");

    // A zero-length frame is a valid frame, not a disconnect.
    ws_send(&mut ws, 0x2, b"");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x2);
    assert!(payload.is_empty());

    // The transport is binary-only.
    ws_send(&mut ws, 0x1, b"text");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1003);
}

#[cfg(unix)]
#[test]
fn plugin_ui_raw_closes_on_an_oversized_message() {
    let (_sandbox, address) = daemon_with_raw_plugin_with(62, "ws_raw_max_bytes = 1024\n");
    let base = plugin_ui_base(&address, "echo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");
    ws_send(&mut ws, 0x2, &vec![0x41; 2048]);
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1009);
}

#[cfg(unix)]
#[test]
fn plugin_ui_raw_reports_protocol_errors_with_a_close_frame() {
    let (_sandbox, address) = daemon_with_raw_plugin(64);
    let base = plugin_ui_base(&address, "echo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}raw", base), &[]);
    assert_eq!(status, 101, "{head}");

    // A frame with reserved bits set is a protocol violation. The client must
    // see the 1002 close, not a bare EOF from the daemon dropping the socket.
    let mut frame = vec![0xF2, 0x80];
    frame.extend_from_slice(&[1, 2, 3, 4]);
    ws.write_all(&frame).expect("reserved-bit frame");
    ws.flush().expect("flush");
    let (opcode, payload) = ws_read(&mut ws);
    assert_eq!(opcode, 0x8, "{}", String::from_utf8_lossy(&payload));
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1002);
}

#[test]
fn plugin_ui_streams_require_lan_auth_and_same_origin() {
    let (mut sandbox, _) = daemon_with_web_event_plugin(53);
    let (address, password) = enable_auth(&mut sandbox);
    let bearer = format!("Bearer {}", password);
    let base = plugin_ui_base_with(&address, "demo", &[("Authorization", bearer.as_str())]);

    let sse = raw_request(&address, "GET", &format!("{}events", base), "", b"");
    assert_eq!(sse.status, 401);
    let (_stream, status, _head) = ws_handshake(&address, &format!("{}ws", base), &[]);
    assert_eq!(status, 401);

    let evil = ws_handshake(
        &address,
        &format!("{}ws", base),
        &[
            ("Authorization", bearer.as_str()),
            ("Origin", "http://evil"),
        ],
    );
    assert_eq!(evil.1, 403);

    let origin = format!("http://{}", address);
    let good = ws_handshake(
        &address,
        &format!("{}ws", base),
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
    let base = plugin_ui_base(&address, "demo");
    let mut first = sse_connect(&address, &format!("{}events", base));
    let call = request(
        &address,
        "POST",
        &format!("{}rpc", base),
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
        &format!("{}rpc", base),
        "application/json",
        br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
    );
    assert_eq!(second.status, 200, "{}", second.body);

    let mut replay = sse_connect_with_last_id(&address, &format!("{}events", base), Some(last_id));
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
    let base = plugin_ui_base(&address, "demo");
    let (mut ws, status, head) = ws_handshake(&address, &format!("{}ws", base), &[]);
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

#[test]
fn http_keep_alive_serves_version_through_the_face() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let mut stream = TcpStream::connect(&address).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");

    let first = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream.write_all(first.as_bytes()).expect("first request");
    let first = read_one_response(&mut stream);
    assert_eq!(first.status, 200);
    assert_eq!(raw_header_values(&first, "connection"), vec!["keep-alive"]);
    let body: Value = serde_json::from_slice(&first.body).expect("json body");
    assert_eq!(body["protocol"], 1);

    let second = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    stream.write_all(second.as_bytes()).expect("second request");
    let second = read_one_response(&mut stream);
    assert_eq!(second.status, 200);
    assert_eq!(raw_header_values(&second, "connection"), vec!["close"]);
}

/// An idle connection that has not sent a request must be closed silently
/// after `http_idle_timeout_secs`; it must not receive a 400 the client never
/// asked for. Both engines close the socket, so the assertion is the same.
#[test]
fn an_idle_connection_is_closed_without_a_response() {
    let mut sandbox = Sandbox::new();
    fs::write(
        sandbox.config.join("svcd.conf"),
        "http_idle_timeout_secs = 1\n",
    )
    .expect("write svcd.conf");
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let mut idle = TcpStream::connect(&address).expect("idle connection");
    idle.set_read_timeout(Some(Duration::from_secs(8)))
        .expect("timeout");
    let mut sink = [0u8; 64];
    loop {
        match idle.read(&mut sink) {
            Ok(0) => break,
            Ok(read) => panic!(
                "an idle connection must be closed, not answered: {:?}",
                String::from_utf8_lossy(&sink[..read])
            ),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                panic!("the daemon did not close an idle connection within 8 s")
            }
            // A reset from the server's close is still a close, not a response.
            Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// S4 thread-budget regression (Linux only).
//
// /proc is the practical Unix way to read another process's thread count; the
// behavior tests stay platform-neutral, so this supplemental check is gated to
// Linux. The default S4 baseline is twelve daemon threads while the legacy
// HTTP workers and takeovers still exist (#116 added the two blocking-pool
// workers). Keep-alive connections and SSE streams are driven by the reactor,
// so neither may add a thread each.
// ---------------------------------------------------------------------------

/// The default S4 topology before the control/plugin reactor merges
/// (#124/#126). #116 added the two blocking-pool workers; the legacy HTTP
/// workers and takeovers stay until #117/#125. Update this pin in the slice
/// that changes the baseline.
#[cfg(target_os = "linux")]
const IDLE_DAEMON_THREADS: usize = 12;

#[cfg(target_os = "linux")]
fn daemon_threads(pid: u32) -> usize {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).expect("daemon status");
    status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|value| value.trim().parse().ok())
        .expect("daemon status has a Threads line")
}

#[cfg(target_os = "linux")]
fn stable_thread_count(pid: u32) -> usize {
    let mut count = daemon_threads(pid);
    let mut stable = 0;
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(20));
        let next = daemon_threads(pid);
        if next == count {
            stable += 1;
            if stable == 5 {
                return count;
            }
        } else {
            count = next;
            stable = 0;
        }
    }
    count
}

#[cfg(target_os = "linux")]
fn open_keep_alive(address: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("keep-alive connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("keep-alive timeout");
    let request = format!(
        "GET /api/v1/version HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    stream
        .write_all(request.as_bytes())
        .expect("keep-alive request");
    let response = read_one_response(&mut stream);
    assert_eq!(response.status, 200, "{}", response.body.len());
    stream
}

#[cfg(target_os = "linux")]
#[test]
fn unix_idle_thread_count_has_no_per_request_or_per_stream_threads() {
    let (sandbox, address) = daemon_with_web_event_plugin(55);
    let pid = sandbox.daemon.as_ref().expect("running daemon").id();

    let idle = stable_thread_count(pid);
    assert_eq!(
        idle, IDLE_DAEMON_THREADS,
        "idle daemon uses {idle} threads; the default S4 baseline is {IDLE_DAEMON_THREADS}, so this is a thread-budget regression"
    );

    let mut connections = Vec::new();
    for _ in 0..16 {
        connections.push(open_keep_alive(&address));
    }
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "16 idle keep-alive connections must not add one daemon thread each"
    );

    let base = plugin_ui_base(&address, "demo");
    let mut streams = Vec::new();
    for _ in 0..8 {
        streams.push(sse_connect(&address, &format!("{}events", base)));
    }
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "8 open SSE streams must not add one daemon thread each"
    );

    drop(streams);
    drop(connections);
    assert_eq!(
        stable_thread_count(pid),
        idle,
        "closing connections and streams must leave no daemon threads behind"
    );
}

// ---------------------------------------------------------------------------
// #108: a live daemon must survive the smallest svcd.conf the parser takes.
//
// limits::tests proves the clamp arithmetic for these keys; this proves the
// running daemon actually uses the clamped values and stays up while each
// surface is exercised. Two configs share one entry point because the raw floor
// of a few keys is a *disabled* feature rather than a smaller one:
//
//   floor  - every key at the smallest value the parser accepts (0 where zero
//            is meaningful, 1 elsewhere). Every surface must answer with a
//            defined status (413 / 400-or-431 / 429 / 503) instead of dying.
//   usable - the same floors except the keys whose raw floor cannot carry one
//            live stream plus the request that feeds it:
//            max_connections / max_connections_per_ip 1 -> 2,
//            max_subscriptions / max_subscribers_per_plugin 0 -> 1,
//            default_timeout 0 -> 1000 ms, because a plugin round trip has to
//            finish inside the call budget before a stream can emit anything.
//            Every buffer and stack size stays at the floor.
//
// Each probe names the svcd.conf key it stresses; a failure prints that key,
// the exact request, the response and the daemon's own output.
// ---------------------------------------------------------------------------

const MIN_CONFIG_PASSWORD: &str = "min-config-password";

/// Every runtime limit svcd.conf accepts, at the parser's raw minimum.
const MINIMAL_LIMITS: &[(&str, &str)] = &[
    ("request_stack_bytes", "1"),
    ("io_stack_bytes", "1"),
    ("blocking_threads", "0"),
    ("max_body_bytes", "1"),
    ("max_json_body_bytes", "1"),
    ("max_complete_body_bytes", "1"),
    ("max_header_bytes", "1"),
    ("max_streams", "0"),
    ("stream_buffer_bytes", "1"),
    ("max_connections", "1"),
    ("max_connections_per_ip", "1"),
    ("max_subscriptions", "0"),
    ("max_subscribers_per_plugin", "0"),
    ("http_idle_timeout_secs", "0"),
    ("log_max_bytes", "1"),
    ("log_rotations", "0"),
    ("max_package_bytes", "1"),
    ("transfer_timeout_secs", "0"),
    ("default_timeout", "0"),
    ("max_timeout", "0"),
    ("ws_max_message_bytes", "1"),
    ("ws_inflight_bytes", "1"),
    ("ws_inflight_requests", "0"),
    ("ws_buffer_shrink_bytes", "1"),
    ("ws_raw_max_bytes", "1"),
    ("ws_raw_queue", "0"),
];

const NO_OVERRIDES: &[(&str, &str)] = &[];

const USABLE_OVERRIDES: &[(&str, &str)] = &[
    ("max_connections", "2"),
    ("max_connections_per_ip", "2"),
    ("max_subscriptions", "1"),
    ("max_subscribers_per_plugin", "1"),
    ("default_timeout", "1000"),
];

fn override_for<'a>(overrides: &'a [(&str, &str)], key: &str) -> Option<&'a str> {
    overrides
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
}

fn minimal_conf(overrides: &[(&str, &str)]) -> String {
    let mut conf =
        String::from("# #108: every limit at its raw minimum; a live daemon must survive it.\n");
    for &(key, floor) in MINIMAL_LIMITS {
        let value = override_for(overrides, key).unwrap_or(floor);
        conf.push_str(&format!("{} = {}\n", key, value));
    }
    conf
}

/// A raw HTTP exchange that reports a dead socket instead of panicking, so a
/// failed probe can still name the svcd.conf key and the request it sent.
struct Exchange {
    status: u16,
    head: String,
    body: Vec<u8>,
    error: Option<String>,
}

impl Exchange {
    fn describe(&self) -> String {
        match &self.error {
            Some(error) => format!("no HTTP response: {error}"),
            None => {
                let body = String::from_utf8_lossy(&self.body);
                let body: String = body.chars().take(400).collect();
                format!("status {}\n{}body: {}", self.status, self.head, body)
            }
        }
    }

    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

fn parse_exchange(raw: &[u8]) -> Exchange {
    let Some(split) = raw.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Exchange {
            status: 0,
            head: String::new(),
            body: Vec::new(),
            error: Some(format!(
                "incomplete response {:?}",
                String::from_utf8_lossy(raw)
            )),
        };
    };
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(0);
    Exchange {
        status,
        head,
        body: raw[split + 4..].to_vec(),
        error: None,
    }
}

/// Write one request and read the response status line first, then the
/// declared body. A server that rejects an oversized request may answer and
/// close while the rest of that request is still in flight, which turns the
/// close into a reset; a client that only calls read_to_end can lose the
/// already-written status line to it. The status line is what the regression
/// asserts, so read it before any reset can matter.
fn exchange(address: &str, head: &str, body: &[u8]) -> Exchange {
    let mut stream = match TcpStream::connect(address) {
        Ok(stream) => stream,
        Err(error) => {
            return Exchange {
                status: 0,
                head: String::new(),
                body: Vec::new(),
                error: Some(format!("connect {address}: {error}")),
            }
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    if let Err(error) = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
    {
        return Exchange {
            status: 0,
            head: String::new(),
            body: Vec::new(),
            error: Some(format!("write: {error}")),
        };
    }

    let mut raw = Vec::new();
    let mut failure = None;
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                if raw.is_empty() {
                    failure = Some("connection closed before a response head".to_string());
                }
                break;
            }
            Ok(_) => {
                raw.push(byte[0]);
                if raw.ends_with(b"\r\n\r\n") {
                    break;
                }
                if raw.len() > 64 * 1024 {
                    failure = Some("response head is too large".to_string());
                    break;
                }
            }
            Err(error) => {
                if raw.is_empty() {
                    failure = Some(format!("read head: {error}"));
                }
                break;
            }
        }
    }
    if failure.is_none() && !raw.is_empty() {
        let text = String::from_utf8_lossy(&raw).to_string();
        let length = text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("content-length") {
                return None;
            }
            value.trim().parse::<usize>().ok()
        });
        if let Some(length) = length {
            let mut declared = vec![0u8; length.min(1024 * 1024)];
            let mut filled = 0;
            while filled < declared.len() {
                match stream.read(&mut declared[filled..]) {
                    Ok(0) => break,
                    Ok(read) => filled += read,
                    // A reset after the head must not hide the status line.
                    Err(_) => break,
                }
            }
            raw.extend_from_slice(&declared[..filled]);
        }
    }
    // Every probe sends Connection: close, so draining to EOF guarantees the
    // server already dropped this connection and freed its (one) admission
    // slot before the next probe connects. A reset here is irrelevant: the
    // status line is already captured.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut sink = [0u8; 512];
    while let Ok(read) = stream.read(&mut sink) {
        if read == 0 {
            break;
        }
    }

    let mut exchange = parse_exchange(&raw);
    exchange.error = exchange.error.or(failure);
    if exchange.error.is_some() {
        exchange.error = Some(format!(
            "{} (received {} bytes)",
            exchange.error.unwrap_or_default(),
            raw.len()
        ));
    }
    exchange
}

fn read_status_line(stream: &mut TcpStream) -> Result<u16, String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.len() > 8192 {
            return Err("response head is too long".to_string());
        }
        match stream.read(&mut byte) {
            Ok(0) => return Err("connection closed before a status line".to_string()),
            Ok(_) => {}
            Err(error) => return Err(format!("read: {error}")),
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {:?}", text))
}

fn tail_text(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn daemon_state(sandbox: &mut Sandbox) -> String {
    match sandbox.daemon.as_mut() {
        None => "no daemon handle".to_string(),
        Some(child) => match child.try_wait() {
            Ok(None) => format!("running (pid {})", child.id()),
            Ok(Some(status)) => format!("EXITED ({status})"),
            Err(error) => format!("try_wait failed: {error}"),
        },
    }
}

fn regression_failure(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    expected: &str,
    actual: &str,
) -> ! {
    let state = daemon_state(sandbox);
    let stderr = tail_text(&sandbox.daemon_log(), 40);
    panic!(
        "minimal-config regression failed\n  svcd.conf key: {key}\n  request: {request}\n  expected: {expected}\n  actual: {actual}\n  daemon: {state}\n  daemon stderr (tail):\n{stderr}"
    )
}

fn expect_status(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    allowed: &[u16],
    response: &Exchange,
) {
    if response.error.is_none() && allowed.contains(&response.status) {
        return;
    }
    regression_failure(
        sandbox,
        key,
        request,
        &format!("status in {allowed:?}"),
        &response.describe(),
    );
}

fn expect_error_code(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    code: &str,
    response: &Exchange,
) {
    if String::from_utf8_lossy(&response.body).contains(code) {
        return;
    }
    regression_failure(
        sandbox,
        key,
        request,
        &format!("an error with code {:?}", code),
        &response.describe(),
    );
}

fn expect_status_code(
    sandbox: &mut Sandbox,
    key: &str,
    request: &str,
    allowed: &[u16],
    status: u16,
    head: &str,
) {
    if allowed.contains(&status) {
        return;
    }
    regression_failure(
        sandbox,
        key,
        request,
        &format!("status in {allowed:?}"),
        &format!("status {status}\n{head}"),
    );
}

fn assert_daemon_alive(sandbox: &mut Sandbox, phase: &str) {
    let state = daemon_state(sandbox);
    if !state.starts_with("running") {
        regression_failure(
            sandbox,
            "liveness",
            &format!("daemon of the {phase} config"),
            "the process still running",
            &state,
        );
    }
    let mut panics: Vec<String> = Vec::new();
    if let Some(line) = sandbox
        .daemon_log()
        .lines()
        .find(|line| line.contains("panicked"))
    {
        panics.push(format!("stderr: {line}"));
    }
    if let Ok(entries) = fs::read_dir(&sandbox.logs) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(text) = fs::read_to_string(&path) {
                if let Some(line) = text.lines().find(|line| line.contains("panicked")) {
                    panics.push(format!("{}: {line}", path.display()));
                }
            }
        }
    }
    if !panics.is_empty() {
        regression_failure(
            sandbox,
            "liveness",
            &format!("daemon of the {phase} config"),
            "no panicked marker in stderr or the log files",
            &panics.join("\n"),
        );
    }
}

/// One MCP request over the Streamable HTTP binding.
fn mcp_min_exchange(
    address: &str,
    password: &str,
    method: &str,
    params: Value,
    name: Option<&str>,
) -> Exchange {
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
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: {}\r\n",
        address, password, method
    );
    if let Some(name) = name {
        head.push_str(&format!("Mcp-Name: {}\r\n", name));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    exchange(address, &head, &body)
}

/// Read SSE events until one contains `needle`, reporting what arrived
/// instead of panicking, so a failure can name the config key and the stream.
fn sse_next_event(
    stream: &mut TcpStream,
    needle: &str,
    timeout: Duration,
) -> Result<String, String> {
    let _ = stream.set_read_timeout(Some(timeout));
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(format!(
                    "stream closed before an event containing {:?}; saw {:?}",
                    needle,
                    String::from_utf8_lossy(&seen)
                ))
            }
            Ok(_) => {
                seen.push(byte[0]);
                if seen.ends_with(b"\n\n") {
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if text.contains(needle) {
                        return Ok(text);
                    }
                    seen.clear();
                }
            }
            Err(error) => {
                return Err(format!(
                    "read: {error}; saw {:?}",
                    String::from_utf8_lossy(&seen)
                ))
            }
        }
    }
}

/// Open an SSE stream, retrying the 429 a previous subscriber can cause while
/// the reactor has not noticed its EOF yet.
fn sse_open_retrying(address: &str, path: &str, last_id: Option<u64>) -> Result<TcpStream, String> {
    let mut last = String::new();
    for _ in 0..80 {
        let mut stream =
            TcpStream::connect(address).map_err(|error| format!("connect: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| format!("timeout: {error}"))?;
        let mut request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n",
            path, address
        );
        if let Some(last_id) = last_id {
            request.push_str(&format!("Last-Event-ID: {}\r\n", last_id));
        }
        request.push_str("\r\n");
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("write: {error}"))?;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream
                .read_exact(&mut byte)
                .map_err(|error| format!("read head: {error}"))?;
            head.push(byte[0]);
        }
        let text = String::from_utf8_lossy(&head).to_string();
        let status = text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(0);
        if status == 200 {
            return Ok(stream);
        }
        if status != 429 {
            return Err(format!("status {status}: {text}"));
        }
        last = text;
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(format!(
        "the previous subscriber was never released: {last}"
    ))
}

#[test]
fn minimal_svcd_conf_keeps_the_daemon_alive() {
    run_min_config_phase("floor", false);
    run_min_config_phase("usable", true);
}

fn run_min_config_phase(phase: &str, streams: bool) {
    let overrides = if streams {
        USABLE_OVERRIDES
    } else {
        NO_OVERRIDES
    };
    let mut sandbox = Sandbox::new();
    fs::write(sandbox.config.join("svcd.conf"), minimal_conf(overrides)).expect("write svcd.conf");

    let key = SigningKey::from_bytes(&[0x6d; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_event_plugin_bundle(&key, "demo");

    sandbox.start_daemon();
    let address = sandbox.wait_for_http();
    let port_before = fs::read_to_string(sandbox.config.join("http.port")).expect("http.port");

    // wait_for_http drops a bare TCP connection. With max_connections = 1 the
    // reactor can still count it when the first real request lands, so retry
    // that one handshake instead of racing the clean-up.
    let mut admitted = false;
    let mut last = String::from("no attempt");
    let warmup = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    for _ in 0..80 {
        let probe = exchange(&address, &warmup, b"");
        if probe.status == 200 {
            admitted = true;
            break;
        }
        last = match &probe.error {
            Some(error) => error.clone(),
            None => format!("status {}", probe.status),
        };
        if probe.status != 503 && probe.error.is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !admitted {
        regression_failure(
            &mut sandbox,
            "max_connections",
            "GET /api/v1/services (the first admitted request)",
            "status 200 within 2 s",
            &last,
        );
    }

    // /mcp always authenticates with the configured password.
    set_password(&sandbox, MIN_CONFIG_PASSWORD);

    // One installed plugin, so the plugin-UI surfaces (WS/SSE) exist at all.
    let installed = exchange(
        &address,
        &request_head(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            package.len(),
            &[],
        ),
        &package,
    );
    expect_status(
        &mut sandbox,
        "max_package_bytes",
        "POST /api/v1/plugins (signed demo package)",
        &[200],
        &installed,
    );
    let started = exchange(
        &address,
        &request_head(&address, "POST", "/api/v1/services/demo/start", "", 0, &[]),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_connections",
        "POST /api/v1/services/demo/start",
        &[200],
        &started,
    );

    // max_package_bytes clamps to 1 MiB. Package installs are a streaming
    // takeover, so the gate is this key (not max_body_bytes, which only bounds
    // the non-tar import body): an over-size Content-Length is refused before a
    // single body byte is read. The compile-time defaults are 8/64/512 MiB.
    let over_package = exchange(
        &address,
        &request_head(
            &address,
            "POST",
            "/api/v1/plugins",
            "application/octet-stream",
            1024 * 1024 + 1,
            &[],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_package_bytes",
        "POST /api/v1/plugins with Content-Length: 1048577 and no body",
        &[413],
        &over_package,
    );
    expect_error_code(
        &mut sandbox,
        "max_package_bytes",
        "POST /api/v1/plugins with Content-Length: 1048577 and no body",
        "payload_too_large",
        &over_package,
    );

    // max_json_body_bytes clamps to 4 KiB on the ordinary JSON routes.
    let over_json = exchange(
        &address,
        &request_head(
            &address,
            "POST",
            "/api/v1/services/demo/start",
            "application/json",
            4 * 1024 + 1,
            &[],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_json_body_bytes",
        "POST /api/v1/services/demo/start with Content-Length: 4097 and no body",
        &[413],
        &over_json,
    );

    // max_header_bytes clamps to 1 KiB: a request just under the clamp parses...
    let pad = "a".repeat(700);
    let under_header = exchange(
        &address,
        &request_head(
            &address,
            "GET",
            "/api/v1/services",
            "",
            0,
            &[("X-Pad", pad.as_str())],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_header_bytes",
        "GET /api/v1/services with a ~800-byte header block",
        &[200],
        &under_header,
    );

    // ...and one just over it is refused. The whole request fits in one read,
    // so nothing is left unread and the status line survives the close.
    // 8/16/64 KiB (the compile-time defaults) would accept ~1.1 KiB, so this is
    // the clamp in action. RFC 9110 asks for 431; the current code answers 400,
    // and #106 owns that conformance change.
    let just_over = "a".repeat(1000);
    let over_header = exchange(
        &address,
        &request_head(
            &address,
            "GET",
            "/api/v1/services",
            "",
            0,
            &[("X-Pad", just_over.as_str())],
        ),
        b"",
    );
    expect_status(
        &mut sandbox,
        "max_header_bytes",
        "GET /api/v1/services with a ~1.1 KiB header block (clamp is 1024)",
        &[400, 431],
        &over_header,
    );

    // A much larger header block is the abortive-close case: the daemon answers
    // and closes while the request is still arriving, so the reset can discard
    // the status line before the client reads it (Windows keeps 1 byte of it).
    // The regression only requires that the daemon survives this input; the
    // status code is pinned by the probe above and #106 owns the RFC 9110
    // 431/graceful-close conformance.
    let big = "a".repeat(4 * 1024);
    let huge_header = exchange(
        &address,
        &request_head(
            &address,
            "GET",
            "/api/v1/services",
            "",
            0,
            &[("X-Pad", big.as_str())],
        ),
        b"",
    );
    if huge_header.status != 0 && !matches!(huge_header.status, 400 | 431) {
        regression_failure(
            &mut sandbox,
            "max_header_bytes",
            "GET /api/v1/services with a 4 KiB header block",
            "an aborted connection or a 400/431 status",
            &huge_header.describe(),
        );
    }
    assert_daemon_alive(&mut sandbox, phase);

    // Concurrent connections: the floor admits one connection and refuses the
    // second with 503 at accept time; the usable config admits two. Both
    // engines enforce this: the Unix reactor counts its connection table, the
    // Windows fallback admits before it spawns the per-connection thread. The
    // second socket is refused without a request, so its 503 is not lost to the
    // abortive close a pending request would cause.
    let mut admitted = TcpStream::connect(&address).expect("admitted connection");
    let mut extra = TcpStream::connect(&address).expect("second connection");
    for stream in [&admitted, &extra] {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
    }
    std::thread::sleep(Duration::from_millis(100));
    let concurrent = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    if streams {
        for (name, stream) in [("first", &mut admitted), ("second", &mut extra)] {
            stream
                .write_all(concurrent.as_bytes())
                .expect("concurrent request");
            let status = read_status_line(stream).unwrap_or_else(|error| {
                regression_failure(
                    &mut sandbox,
                    "max_connections",
                    &format!("{name} concurrent connection"),
                    "status 200",
                    &error,
                )
            });
            if status != 200 {
                regression_failure(
                    &mut sandbox,
                    "max_connections / max_connections_per_ip",
                    &format!("{name} concurrent connection"),
                    "status 200",
                    &format!("status {status}"),
                );
            }
        }
    } else {
        let refused = read_status_line(&mut extra).unwrap_or_else(|error| {
            regression_failure(
                &mut sandbox,
                "max_connections",
                "second concurrent connection (refused at accept, no request sent)",
                "a 503 status line",
                &error,
            )
        });
        if refused != 503 {
            regression_failure(
                &mut sandbox,
                "max_connections",
                "second concurrent connection (refused at accept, no request sent)",
                "status 503",
                &format!("status {refused}"),
            );
        }
        admitted
            .write_all(concurrent.as_bytes())
            .expect("admitted request");
        let served = read_status_line(&mut admitted).unwrap_or_else(|error| {
            regression_failure(
                &mut sandbox,
                "max_connections",
                "first (admitted) concurrent connection",
                "status 200",
                &error,
            )
        });
        if served != 200 {
            regression_failure(
                &mut sandbox,
                "max_connections",
                "first (admitted) concurrent connection",
                "status 200",
                &format!("status {served}"),
            );
        }
    }

    // http_idle_timeout_secs clamps to 1 s: the Unix reactor's idle sweep and
    // the Windows fallback's per-socket read timeout both close a keep-alive
    // connection instead of idling for the compile-time default (30/60/120 s).
    let mut idle = TcpStream::connect(&address).expect("idle connection");
    idle.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    idle.write_all(
        format!(
            "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
            address
        )
        .as_bytes(),
    )
    .expect("keep-alive request");
    let kept = read_one_response(&mut idle);
    if kept.status != 200 {
        regression_failure(
            &mut sandbox,
            "http_idle_timeout_secs",
            "keep-alive GET /api/v1/services",
            "status 200 on the keep-alive connection",
            &format!("status {}", kept.status),
        );
    }
    let opened = Instant::now();
    let mut closed = false;
    let mut sink = [0u8; 128];
    while opened.elapsed() < Duration::from_secs(8) {
        match idle.read(&mut sink) {
            Ok(0) => {
                closed = true;
                break;
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    if !closed {
        regression_failure(
            &mut sandbox,
            "http_idle_timeout_secs",
            "keep-alive GET /api/v1/services left idle",
            "the daemon closes an idle connection within ~1 s",
            "still open after 8 s",
        );
    }

    // MCP over Streamable HTTP: tools/list and tools/call. The modern binding
    // has no initialize handshake, so that method must answer 404 -32601
    // rather than drop the connection.
    let listed = mcp_min_exchange(&address, MIN_CONFIG_PASSWORD, "tools/list", json!({}), None);
    expect_status(
        &mut sandbox,
        "max_json_body_bytes",
        "POST /mcp tools/list",
        &[200],
        &listed,
    );
    let listed = listed.json().unwrap_or_else(|| {
        regression_failure(
            &mut sandbox,
            "max_json_body_bytes",
            "POST /mcp tools/list",
            "a JSON-RPC response",
            &listed.describe(),
        )
    });
    let names = tool_names(&listed["result"]["tools"]);
    if !names.contains(&"svc.health".to_string()) {
        regression_failure(
            &mut sandbox,
            "max_json_body_bytes",
            "POST /mcp tools/list",
            "svc.health in result.tools",
            &format!("tools {names:?}"),
        );
    }

    let called = mcp_min_exchange(
        &address,
        MIN_CONFIG_PASSWORD,
        "tools/call",
        json!({"name": "svc.health", "arguments": {}}),
        Some("svc.health"),
    );
    expect_status(
        &mut sandbox,
        "default_timeout",
        "POST /mcp tools/call svc.health (100 ms budget at the floor)",
        &[200],
        &called,
    );
    let called = called.json().unwrap_or_else(|| {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "POST /mcp tools/call svc.health",
            "a JSON-RPC response",
            &called.describe(),
        )
    });
    if called["result"].is_null() || called["result"]["isError"] == json!(true) {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "POST /mcp tools/call svc.health (100 ms budget at the floor)",
            "a successful result",
            &called.to_string(),
        );
    }

    let initialize = mcp_min_exchange(&address, MIN_CONFIG_PASSWORD, "initialize", json!({}), None);
    expect_status(
        &mut sandbox,
        "max_json_body_bytes",
        "POST /mcp initialize (not spoken on the modern binding)",
        &[404],
        &initialize,
    );

    // The stdio bridge still speaks the legacy handshake; it reaches the same
    // daemon over the control channel, not over HTTP.
    let bridged = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "min-config", "version": "1"}
        }
    }));
    if bridged["result"]["serverInfo"]["name"] != json!("svcd") {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "stdio bridge initialize",
            "result.serverInfo.name = svcd",
            &bridged.to_string(),
        );
    }
    let bridged_list = sandbox.mcp_stdio(json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {"_meta": modern_meta()}
    }));
    let bridged_names = tool_names(&bridged_list["result"]["tools"]);
    if !bridged_names.contains(&"svc.health".to_string()) {
        regression_failure(
            &mut sandbox,
            "default_timeout",
            "stdio bridge tools/list",
            "svc.health in result.tools",
            &format!("tools {bridged_names:?}"),
        );
    }

    let base = plugin_ui_base(&address, "demo");
    let ws_path = format!("{}ws", base);
    let events_path = format!("{}events", base);

    if streams {
        let (mut ws, status, head) = ws_handshake(&address, &ws_path, &[]);
        expect_status_code(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>ws (Upgrade: websocket)",
            &[101],
            status,
            &head,
        );
        let call = json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "tools/call",
            "params": {"name": "status", "arguments": {}}
        });
        ws_send(&mut ws, 0x1, call.to_string().as_bytes());
        let mut answered = false;
        for _ in 0..8 {
            let (opcode, payload) = ws_read(&mut ws);
            if opcode != 0x1 {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
                continue;
            };
            if value["id"] == json!(21) && value["result"]["content"][0]["text"] == json!("pong") {
                answered = true;
                break;
            }
        }
        if !answered {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "WebSocket tools/call status",
                "a JSON-RPC result frame with id 21",
                "no matching frame",
            );
        }

        // The plugin's own notification for the previous call is still queued,
        // so skip text frames until the pong arrives.
        ws_send(&mut ws, 0x9, b"min-config");
        let mut pong = None;
        for _ in 0..8 {
            let (opcode, payload) = ws_read(&mut ws);
            if opcode == 0xA {
                pong = Some(payload);
                break;
            }
        }
        if pong.as_deref() != Some(b"min-config".as_ref()) {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "WebSocket ping frame",
                "a pong frame echoing the payload",
                &format!("pong {pong:?}"),
            );
        }

        // ws_max_message_bytes clamps to 1 KiB: a larger message is closed with
        // 1009 instead of growing the buffer or dropping the socket silently.
        ws_send(&mut ws, 0x1, &vec![b'x'; 2048]);
        let mut close = None;
        for _ in 0..8 {
            let (opcode, payload) = ws_read(&mut ws);
            if opcode == 0x8 {
                close = Some(payload);
                break;
            }
        }
        let code = close
            .as_ref()
            .and_then(|payload| payload.get(0..2))
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
            .unwrap_or(0);
        if code != 1009 {
            regression_failure(
                &mut sandbox,
                "ws_max_message_bytes",
                "WebSocket text frame of 2048 bytes (clamp is 1024)",
                "a close frame with code 1009",
                &format!("close {close:?}"),
            );
        }
        drop(ws);

        // SSE: connect, receive one event, then reconnect with Last-Event-ID and
        // receive the event published while no subscriber was attached.
        // Retry a 429 here too: the WS probe's subscription is released by the
        // server a poll cycle after its socket closes, and the usable config
        // allows exactly one subscriber.
        let mut sse = sse_open_retrying(&address, &events_path, None).unwrap_or_else(|error| {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events (SSE)",
                "status 200 within 2 s",
                &error,
            )
        });
        let rpc_path = format!("{}rpc", base);
        let call = br#"{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let answered = exchange(
            &address,
            &request_head(
                &address,
                "POST",
                &rpc_path,
                "application/json",
                call.len(),
                &[],
            ),
            call,
        );
        expect_status(
            &mut sandbox,
            "max_connections_per_ip",
            "POST <base>rpc while the SSE stream holds the other connection",
            &[200],
            &answered,
        );
        if !String::from_utf8_lossy(&answered.body).contains("pong") {
            regression_failure(
                &mut sandbox,
                "default_timeout",
                "POST <base>rpc tools/call status (plugin round trip)",
                "a result containing pong",
                &answered.describe(),
            );
        }
        let event = sse_next_event(&mut sse, "pong-event", Duration::from_secs(5)).unwrap_or_else(
            |error| {
                regression_failure(
                    &mut sandbox,
                    "max_subscribers_per_plugin",
                    "GET <base>events (SSE) after one tools/call",
                    "a notifications/message event",
                    &error,
                )
            },
        );
        if !event.contains("notifications/message") {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events (SSE)",
                "a notifications/message event",
                &event,
            );
        }
        let last_id = sse_id(&event).unwrap_or_else(|| {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events (SSE)",
                "an id line on the event",
                &event,
            )
        });
        drop(sse);

        let later = br#"{"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let published = exchange(
            &address,
            &request_head(
                &address,
                "POST",
                &rpc_path,
                "application/json",
                later.len(),
                &[],
            ),
            later,
        );
        expect_status(
            &mut sandbox,
            "max_connections_per_ip",
            "POST <base>rpc with no SSE subscriber attached",
            &[200],
            &published,
        );

        let mut replay =
            sse_open_retrying(&address, &events_path, Some(last_id)).unwrap_or_else(|error| {
                regression_failure(
                    &mut sandbox,
                    "max_subscribers_per_plugin",
                    "GET <base>events with Last-Event-ID",
                    "status 200 within 2 s",
                    &error,
                )
            });
        let replayed = sse_next_event(&mut replay, "pong-event", Duration::from_secs(5))
            .unwrap_or_else(|error| {
                regression_failure(
                    &mut sandbox,
                    "max_subscribers_per_plugin",
                    "GET <base>events with Last-Event-ID",
                    "the replayed event within 5 s",
                    &error,
                )
            });
        let replayed_id = sse_id(&replayed).unwrap_or_else(|| {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events with Last-Event-ID",
                "an id line on the replayed event",
                &replayed,
            )
        });
        if replayed_id <= last_id {
            regression_failure(
                &mut sandbox,
                "max_subscribers_per_plugin",
                "GET <base>events with Last-Event-ID",
                &format!("a replayed event after id {last_id}"),
                &format!("replayed id {replayed_id}: {replayed}"),
            );
        }
        drop(replay);
    } else {
        // The floor disables UI subscribers outright: 0 is a feature switch,
        // and both stream surfaces must say so instead of dropping the socket.
        let upgrade = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
            ws_path,
            address,
            ws_key()
        );
        let handshake = exchange(&address, &upgrade, b"");
        expect_status(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>ws (Upgrade: websocket, zero subscribers allowed)",
            &[429],
            &handshake,
        );
        let events = exchange(
            &address,
            &request_head(
                &address,
                "GET",
                &events_path,
                "",
                0,
                &[("Accept", "text/event-stream")],
            ),
            b"",
        );
        expect_status(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>events (SSE, zero subscribers allowed)",
            &[429],
            &events,
        );
        expect_error_code(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>events (SSE, zero subscribers allowed)",
            "too_many_subscribers",
            &events,
        );
    }

    assert_daemon_alive(&mut sandbox, phase);
    let port_after = fs::read_to_string(sandbox.config.join("http.port")).expect("http.port");
    if port_after != port_before {
        regression_failure(
            &mut sandbox,
            "liveness",
            &format!("http.port of the {phase} config"),
            &format!("{port_before:?} (the daemon must not restart)"),
            &port_after,
        );
    }
}
