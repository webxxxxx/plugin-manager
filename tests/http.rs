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

#[path = "http/auth.rs"]
mod auth;
#[path = "http/config.rs"]
mod config;
#[path = "http/control.rs"]
mod control;
#[path = "http/install.rs"]
mod install;
#[path = "http/lifecycle.rs"]
mod lifecycle;
#[path = "http/mcp.rs"]
mod mcp;
#[path = "http/protocol.rs"]
mod protocol;
mod support;
#[path = "http/transfer.rs"]
mod transfer;
#[path = "http/ui.rs"]
mod ui;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn sandbox_base() -> std::path::PathBuf {
    // Unix domain sockets cap the whole path at ~108 bytes, so Unix keeps the
    // short system temp. Windows needs a target-local directory: a restricted
    // %TEMP% ACL made sandbox creation fail there.
    if cfg!(windows) {
        return std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    }
    std::env::temp_dir()
}

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
    /// Overridable so the bounded-wait test does not spend the production bound.
    stop_timeout: Duration,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = sandbox_base().join(format!(
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
        support::install_multicall(Path::new(binary), &[&svcd, &svc, &mcp]);

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
            stop_timeout: support::STOP_TIMEOUT,
        }
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
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
            .env("SVCD_TEST_UNPRIVILEGED", "1")
            .env("SVCD_RESCAN_INTERVAL_MS", "0")
            .env("SVCD_BOOT_SCAN_WAIT_MS", "0");
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    fn wait_for_http(&self) -> String {
        for _ in 0..200 {
            if let Ok(address) = fs::read_to_string(self.data.join("run").join("http.port")) {
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

    /// Stop the daemon with a bounded request and a bounded wait; see
    /// [`support::stop_daemon`]. `child.wait()` alone could hang the suite
    /// when the request never reached the daemon.
    fn stop_daemon(&mut self) -> Result<(), String> {
        let config = self.config.clone();
        self.stop_daemon_at(&config)
    }

    /// Stop, but target `config` instead of the sandbox's own. The control
    /// socket (Unix) and the pipe name (Windows) both derive from this string,
    /// so a dir no daemon bound makes the request genuinely unreachable.
    fn stop_daemon_at(&mut self, config: &Path) -> Result<(), String> {
        let Some(mut child) = self.daemon.take() else {
            return Ok(());
        };
        let log = self.daemon_stderr.clone();
        let mut command = Command::new(&self.svcd);
        command.arg("shutdown");
        self.env(&mut command);
        command.env("SVCD_CONFIG_DIR", config);
        // A different config is the "unreachable" probe: on Unix the control
        // socket is under data/run and on Windows the pipe name is derived from
        // config_dir, so move both. The normal stop keeps the sandbox roots.
        if config != self.config {
            command.env("SVCD_DATA_DIR", config.join("no-data"));
        }
        support::stop_daemon(&mut child, &mut command, self.stop_timeout, || {
            fs::read_to_string(&log).unwrap_or_default()
        })
    }

    fn shutdown(&mut self) {
        support::report_stop(self.stop_daemon());
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
        // A red in this target otherwise loses the daemon side entirely: the
        // harness keeps the service log but never the supervisor's stderr.
        if std::thread::panicking() {
            let log = self.daemon_log();
            if !log.is_empty() {
                eprintln!("daemon log:\n{}", log);
            }
        }
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

fn parse_raw_response(response: Vec<u8>) -> RawResponse {
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

fn read_raw_response(stream: &mut TcpStream) -> RawResponse {
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    parse_raw_response(response)
}

/// Send one hand-built request head with no body and read the full response.
/// Used when the test must declare a framing the helpers cannot express; the
/// bounded read reports the daemon log on failure, per the suite's convention.
fn send_request_head(sandbox: &Sandbox, address: &str, head: &str) -> RawResponse {
    let mut stream = TcpStream::connect(address)
        .unwrap_or_else(|error| panic!("connect {address}: {error}\n{}", sandbox.daemon_log()));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap_or_else(|error| panic!("read timeout: {error}\n{}", sandbox.daemon_log()));
    stream
        .write_all(head.as_bytes())
        .unwrap_or_else(|error| panic!("write request head: {error}\n{}", sandbox.daemon_log()));
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .unwrap_or_else(|error| panic!("read response: {error}\n{}", sandbox.daemon_log()));
    parse_raw_response(response)
}

fn read_one_response(stream: &mut TcpStream) -> RawResponse {
    let (status, headers) = read_response_head(stream);
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
    while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
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

/// A signed package with one extra large payload file. Used by the export
/// tests so the download is still in flight while the client stalls.
fn plugin_bundle_with_payload(
    key: &SigningKey,
    id: &str,
    version: &str,
    payload: &[u8],
) -> Vec<u8> {
    let (manifest, mut files) = plugin_parts(id, version, None, false);
    files.push(("payload/big.bin".to_string(), payload.to_vec(), 0o644));
    files.sort_by(|left, right| left.0.cmp(&right.0));
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
    unsigned_backup_with_schema(id, 1)
}

fn unsigned_backup_with_schema(id: &str, schema: u32) -> Vec<u8> {
    let spk = unsigned_plugin_bundle(id);
    let backup = serde_json::to_vec(&json!({"schema": schema, "id": id, "version": "1.0.0"}))
        .expect("backup");
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
        ("payload/web/large.html".to_string(), large_web_page(), 0o644),
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

/// A page larger than the default stream buffer. Serving it end to end
/// proves the stream handler chunks its memory body instead of growing the
/// output buffer to the whole response.
fn large_web_page() -> Vec<u8> {
    let mut html =
        Vec::from(&b"<!doctype html>\n<html><head><title>large</title></head><body>\n"[..]);
    for line in 0..400 {
        html.extend_from_slice(
            format!(
                "<p>row {}: the plugin UI stream stays inside the configured buffer</p>\n",
                line
            )
            .as_bytes(),
        );
    }
    html.extend_from_slice(b"</body></html>\n");
    html
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
  if ($line -match '"name"\s*:\s*"slow"') {
    Start-Sleep -Seconds 5
  }
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
  case "$line" in
    *'"name":"slow"'*) sleep 5 ;;
  esac
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
            {"name": "status", "desc": "sample status", "inputSchema": {"type": "object"}},
            {"name": "slow", "desc": "slow status", "inputSchema": {"type": "object"}}
        ],
        "ui": {"type": "web", "entry": "index.html"},
    });
    signed_package(key, manifest, &files)
}

fn daemon_with_web_event_plugin(key_byte: u8) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    // This UI test holds 16 keep-alive connections, 8 SSE streams and 16
    // concurrent page requests, all from 127.0.0.1, and its probes retry. State
    // the per-address cap instead of taking the shape's share: the point of the
    // test is threads, and a small host's 64-connection cap would refuse the
    // page load for a reason this test is not about.
    fs::write(
        sandbox.config.join("svcd.conf"),
        "max_connections_per_ip = 128\n",
    )
    .expect("write svcd.conf");
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

/// A signed plugin whose post-install hook writes `marker` and then sleeps for
/// five seconds. The hook runs on the supervisor loop, so it holds the
/// `plugin.install_staged` control round trip open long enough to disconnect.
#[cfg(unix)]
fn slow_hook_plugin_bundle(key: &SigningKey, marker: &Path) -> Vec<u8> {
    let exec_path = "payload/bin/agent.sh".to_string();
    let hook_path = "hooks/post-install.sh".to_string();
    let hook = format!("#!/bin/sh\n: > '{}'\nsleep 5\n", marker.display());
    let files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), b"#!/bin/sh\nexit 0\n".to_vec(), 0o755),
        (hook_path.clone(), hook.into_bytes(), 0o755),
    ];
    let manifest = json!({
        "schema": 1,
        "name": "slowhook",
        "version": "1.0.0",
        "desc": "slow post-install hook",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "hooks": {"postInstall": hook_path},
    });
    signed_package(key, manifest, &files)
}

fn ws_key() -> String {
    BASE64.encode([0x42u8; 16])
}

/// Open a WebSocket handshake and return (stream, status, response head).
fn ws_handshake(address: &str, path: &str, extra: &[(&str, &str)]) -> (TcpStream, u16, String) {
    try_ws_handshake(address, path, extra)
        .unwrap_or_else(|error| panic!("ws handshake {path}: {error}"))
}

/// The fallible form: an admission refusal is a transport failure, not a
/// protocol verdict, so the caller can retry it.
fn try_ws_handshake(
    address: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> Result<(TcpStream, u16, String), String> {
    let mut stream = TcpStream::connect(address).map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("timeout: {error}"))?;
    let key = ws_key();
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n",
        path, address, key
    );
    for (name, value) in extra {
        request.push_str(&format!("{}: {}\r\n", name, value));
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
    let head = String::from_utf8_lossy(&head).to_string();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| format!("bad status line {:?}", head))?;
    Ok((stream, status, head))
}

/// A handshake that has to reach the handler: retry until the daemon admits the
/// connection instead of panicking on the admission refusal (#155). A readable
/// 503 is the same refusal as a reset one: the gate answers busy without a
/// handler, so it is retried too.
fn ws_handshake_admitted(
    sandbox: &mut Sandbox,
    address: &str,
    path: &str,
    extra: &[(&str, &str)],
) -> (TcpStream, u16, String) {
    let deadline = Instant::now() + ADMISSION_WAIT;
    loop {
        match try_ws_handshake(address, path, extra) {
            Ok((stream, 503, _head)) => {
                drop(stream);
                if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
                    regression_failure(
                        sandbox,
                        "max_connections",
                        &format!("GET {path} (WebSocket handshake)"),
                        "an admitted handshake within the admission wait",
                        "the admission gate kept answering 503",
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(handshake) => return handshake,
            Err(error) => {
                if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
                    regression_failure(
                        sandbox,
                        "max_connections",
                        &format!("GET {path} (WebSocket handshake)"),
                        "an answered handshake within the admission wait",
                        &error,
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
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

/// Open an SSE request and return its status without waiting for the stream
/// to end. Used when a long-lived response may be refused or accepted.
fn sse_probe(address: &str, path: &str) -> (u16, TcpStream) {
    let mut stream = TcpStream::connect(address).expect("sse connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("sse timeout");
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/event-stream\r\n\r\n",
        path, address
    );
    stream.write_all(request.as_bytes()).expect("sse request");
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("sse response head");
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .expect("sse status");
    (status, stream)
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

/// A running daemon with a plugin whose payload is large enough that an
/// export cannot finish inside one socket buffer.
fn export_sandbox(key_byte: u8, extra: &str, payload: &[u8]) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = plugin_bundle_with_payload(&key, "demo", "1.0.0", payload);
    sandbox.allow_insecure_install_with(extra);
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

/// Like `web_ui_sandbox`, with extra `svcd.conf` keys for one test.
fn web_ui_sandbox_with_conf(key_byte: u8, extra: &str) -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[key_byte; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    let package = web_plugin_bundle(&key, "demo", true);
    sandbox.allow_insecure_install_with(extra);
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

/// Read exactly one response head. A HEAD response must put the next response
/// immediately after this terminator, with no body bytes in between.
fn read_response_head_raw(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    head
}

fn parse_response_head(head: &[u8]) -> (u16, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.lines();
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
    (status, headers)
}

fn read_response_head(stream: &mut TcpStream) -> (u16, Vec<(String, String)>) {
    parse_response_head(&read_response_head_raw(stream))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// A stalled download must hold one long-lived max_streams slot, keep the
/// server loop free, and release the slot when the client disconnects. Both
/// ends of the connection stay on the server loop; the client simply stops
/// reading after the head.
fn assert_slow_download_holds_the_cap(address: &str, path: &str, content: &[u8]) {
    let mut stalled = TcpStream::connect(address).expect("stalled connect");
    stalled
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled timeout");
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, address
    );
    stalled
        .write_all(request.as_bytes())
        .expect("stalled request");
    let (status, _) = read_response_head(&mut stalled);
    assert_eq!(status, 200);

    // The loop still answers an ordinary request while the transfer is stalled.
    let version = raw_request(address, "GET", "/api/v1/version", "", b"");
    assert_eq!(version.status, 200, "a slow reader must not block the loop");

    // The stalled transfer owns the only long-lived slot.
    let busy = raw_request(address, "GET", path, "", b"");
    assert_eq!(busy.status, 503, "{:?}", busy.body);
    assert_eq!(
        raw_header_values(&busy, "retry-after"),
        vec!["1"],
        "the busy response keeps the stable Retry-After contract"
    );
    assert_eq!(json_body(&busy)["error"]["code"], "busy");

    // A HEAD download refused by the same budget carries GET's head without a
    // body: the JSON envelope must not stay on the wire.
    let get_busy_length = header_value(&busy.headers, "content-length")
        .expect("GET 503 advertises its error body")
        .to_string();
    let head_busy = raw_request(address, "HEAD", path, "", b"");
    assert_eq!(head_busy.status, 503);
    assert!(
        head_busy.body.is_empty(),
        "HEAD 503 carried a body: {}",
        String::from_utf8_lossy(&head_busy.body)
    );
    assert_eq!(
        header_value(&head_busy.headers, "content-length"),
        Some(get_busy_length.as_str()),
        "HEAD 503 keeps the GET Content-Length"
    );
    assert_eq!(raw_header_values(&head_busy, "retry-after"), vec!["1"]);

    // Releasing the stalled connection releases the slot.
    drop(stalled);
    let mut released = None;
    for _ in 0..200 {
        let response = raw_request(address, "GET", path, "", b"");
        if response.status == 200 {
            released = Some(response);
            break;
        }
        assert_eq!(response.status, 503, "{:?}", response.body);
        std::thread::sleep(Duration::from_millis(10));
    }
    let released = released.expect("the stream cap must be released after a disconnect");
    assert_eq!(released.body.len(), content.len());
    assert_eq!(released.body, content);
}

/// Larger than any plausible loopback socket buffer, so the transfer is still
/// in progress while the client refuses to read.
fn large_download_body() -> Vec<u8> {
    vec![0x5a; 32 * 1024 * 1024]
}

fn has_staging(dir: &Path) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".upload-") || name.starts_with(".part-") {
                return true;
            }
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                stack.push(entry.path());
            }
        }
    }
    false
}

fn wait_until<F, D>(sandbox: &Sandbox, what: &str, ready: F, details: D)
where
    F: FnMut() -> bool,
    D: FnMut() -> String,
{
    wait_until_within(sandbox, what, Duration::from_secs(3), ready, details)
}

/// The bounded form for a wait that legitimately outlives the default three
/// seconds (a lifecycle hook, for example). It keeps the daemon-log tail on
/// timeout instead of a bare sleep loop.
fn wait_until_within<F, D>(
    sandbox: &Sandbox,
    what: &str,
    timeout: Duration,
    mut ready: F,
    mut details: D,
) where
    F: FnMut() -> bool,
    D: FnMut() -> String,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let extra = details();
    panic!("{what}\ndaemon log:\n{}\n{extra}", sandbox.daemon_log());
}

fn wait_for_staging(sandbox: &Sandbox, dir: &Path) {
    wait_until(
        sandbox,
        &format!("upload staging file did not appear under {}", dir.display()),
        || has_staging(dir),
        String::new,
    );
}

fn wait_for_no_staging(sandbox: &Sandbox, dir: &Path) {
    wait_until(
        sandbox,
        &format!(
            "upload staging file was not removed under {}",
            dir.display()
        ),
        || !has_staging(dir),
        String::new,
    );
}

/// A package install/import stages into a hidden directory directly under the
/// plugin tree, not into the blob staging area.
fn has_package_staging(plugins: &Path) -> bool {
    fs::read_dir(plugins)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".staging-"))
        })
        .unwrap_or(false)
}

fn wait_for_package_staging_with<D>(sandbox: &Sandbox, plugins: &Path, details: D)
where
    D: FnMut() -> String,
{
    wait_until(
        sandbox,
        &format!(
            "package staging directory did not appear under {}",
            plugins.display()
        ),
        || has_package_staging(plugins),
        details,
    );
}

#[cfg_attr(windows, allow(dead_code))] // only the Unix thread-budget test drives it
fn wait_for_package_staging(sandbox: &Sandbox, plugins: &Path) {
    wait_for_package_staging_with(sandbox, plugins, String::new);
}

fn wait_for_no_package_staging(sandbox: &Sandbox, plugins: &Path) {
    wait_until(
        sandbox,
        &format!(
            "package staging directory was not removed under {}",
            plugins.display()
        ),
        || !has_package_staging(plugins),
        String::new,
    );
}

/// Open an upload and send only the head plus a first chunk. The server has a
/// valid content-length but the body is deliberately left unfinished.
fn start_stalled_upload(
    address: &str,
    method: &str,
    path: &str,
    declared: usize,
    initial: &[u8],
) -> TcpStream {
    let mut stream = TcpStream::connect(address).expect("stalled upload connect");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("stalled upload write timeout");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("stalled upload read timeout");
    let head = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
        method, path, address, declared
    );
    stream.write_all(head.as_bytes()).expect("upload head");
    stream.write_all(initial).expect("upload initial body");
    stream
}

fn second_upload_status(address: &str) -> (u16, Vec<(String, String)>) {
    let mut stream = TcpStream::connect(address).expect("second upload connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("second upload timeout");
    let head = format!(
        "POST /api/v1/blobs HTTP/1.1\r\nHost: {}\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
        address
    );
    stream
        .write_all(head.as_bytes())
        .expect("second upload head");
    read_response_head(&mut stream)
}

fn signed_demo_package(sandbox: &Sandbox, seed: u8) -> Vec<u8> {
    let key = SigningKey::from_bytes(&[seed; 32]);
    fs::write(
        sandbox.trust.join("test.pub"),
        pem_from_public(&key.verifying_key().to_bytes()),
    )
    .expect("trust key");
    plugin_bundle(&key, "demo", "1.0.0", Some(b"good template\n"), false)
}

const TEST_PASSWORD: &str = "test-password";
const TEST_PASSWORD_2: &str = "new-test-password";

fn set_password(sandbox: &Sandbox, password: &str) {
    let changed = sandbox.run_svc(&["lan", "passwd", password]);
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

fn lan_status_field(text: &str, prefix: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("missing {prefix:?} in the control reply: {text:?}"))
        .trim()
        .to_string()
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

fn rpc_body(id: u64, name: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": name, "arguments": {}}
    }))
    .expect("rpc body")
}

// ---------------------------------------------------------------------------
// S4 thread-budget regression (Linux only).
//
// /proc is the practical Unix way to read another process's thread count; the
// behavior tests stay platform-neutral, so this supplemental check is gated to
// Linux. #126 merged the plugin stdio reactor into the supervisor loop, so the
// default S4 baseline is four: the main/supervisor thread, the server loop and
// two blocking-pool workers. Keep-alive connections and SSE streams are driven
// by the server loop, so none of them may add a thread each.
// ---------------------------------------------------------------------------

/// The default S4 topology: supervisor (which also polls plugin stdio), server
/// loop and the two blocking workers. Update this pin in the slice that changes
/// the baseline.
#[cfg(target_os = "linux")]
const IDLE_DAEMON_THREADS: usize = 4;

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
fn daemon_thread_names(pid: u32) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(format!("/proc/{}/task", pid))
        .expect("daemon tasks")
        .flatten()
        .filter_map(|entry| fs::read_to_string(entry.path().join("comm")).ok())
        .map(|name| name.trim().to_string())
        .collect();
    names.sort();
    names
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
    ("pending_queue", "0"),
    ("progress_queue", "0"),
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
    // The read failure is the more specific diagnosis: with an empty buffer
    // `parse_exchange` can only say "incomplete response", hiding the reset or
    // read error that actually happened.
    exchange.error = failure.or(exchange.error);
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

/// Bounded wait for the daemon to admit a probe.
///
/// The minimal config runs with `max_connections = 1`, which makes admission
/// asynchronous: the daemon frees a slot only once the previous handler thread
/// has returned, and a refused connection is closed without reading the request,
/// so Windows turns that close into a reset that can discard the busy reply.
/// "No answer" and "503 busy" therefore both mean "not admitted yet" on the
/// client side, and a loaded machine widens the window; wait it out against a
/// deadline instead of a fixed 2 s cap (#155).
const ADMISSION_WAIT: Duration = Duration::from_secs(30);

/// True when the probe never reached a handler: the admission gate answered
/// busy, or the connection was reset before the response head. A complete
/// non-2xx answer (a real 4xx/5xx from a handler) is *not* a refusal, so the
/// retry loops return it immediately and report it as a failure.
fn admission_refused(probe: &Exchange) -> bool {
    probe.status == 0 || probe.status == 503 || probe.error.is_some()
}

/// One request that must be answered by a handler, not by the admission gate.
/// Retries an admission refusal against [`ADMISSION_WAIT`]; a stopped daemon is
/// the bail-out, so a crash is reported instead of waited on.
fn exchange_admitted(sandbox: &mut Sandbox, address: &str, head: &str, body: &[u8]) -> Exchange {
    let deadline = Instant::now() + ADMISSION_WAIT;
    loop {
        let probe = exchange(address, head, body);
        if !admission_refused(&probe)
            || Instant::now() >= deadline
            || !daemon_state(sandbox).starts_with("running")
        {
            return probe;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The floor admits exactly one of two concurrent connections, the usable
/// config both. The pair is opened before either socket sends, so it races the
/// slot release of whatever probe came before; retry the whole pair until it
/// shows the configured shape (#155). The refused socket carries no request,
/// so its 503 is not lost to the reset a pending request would cause.
fn assert_concurrent_connections(sandbox: &mut Sandbox, address: &str, streams: bool) {
    let request = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    let deadline = Instant::now() + ADMISSION_WAIT;
    let mut last = String::from("no attempt");
    loop {
        let mut first = connect_for_probe(
            sandbox,
            "max_connections",
            "first concurrent connection",
            address,
        );
        let mut second = connect_for_probe(
            sandbox,
            "max_connections",
            "second concurrent connection",
            address,
        );
        for stream in [&first, &second] {
            if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
                regression_failure(
                    sandbox,
                    "max_connections",
                    "a concurrent connection",
                    "a socket with a read timeout",
                    &format!("read timeout: {error}"),
                );
            }
        }
        // Let the accept loop admit (or refuse) both sockets before either
        // writes, so the refusal arrives with no unread request data.
        std::thread::sleep(Duration::from_millis(100));
        let mut accepted = true;
        if streams {
            for (name, stream) in [("first", &mut first), ("second", &mut second)] {
                if let Err(error) = stream.write_all(request.as_bytes()) {
                    // Both sockets can be refused before the pair is retried.
                    last = format!("{name} concurrent connection: write: {error}");
                    accepted = false;
                    continue;
                }
                match read_status_line(stream) {
                    Ok(200) => {}
                    // 503 is the admission gate answering; wait for a slot.
                    Ok(503) => {
                        last = format!("{name} concurrent connection: status 503");
                        accepted = false;
                    }
                    // Any other complete answer is a real failure, not a
                    // refusal: report it now instead of retrying it away.
                    Ok(status) => regression_failure(
                        sandbox,
                        "max_connections",
                        &format!("{name} concurrent connection"),
                        "status 200",
                        &format!("status {status}"),
                    ),
                    Err(error) => {
                        last = format!("{name} concurrent connection: {error}");
                        accepted = false;
                    }
                }
            }
            if accepted {
                return;
            }
        } else {
            match read_status_line(&mut second) {
                Ok(503) => {
                    // The second socket was refused at accept, so the first one
                    // holds the only slot. A write can still fail when both
                    // sockets were refused before the pair was retried.
                    if let Err(error) = first.write_all(request.as_bytes()) {
                        last = format!("first (admitted) concurrent connection: write: {error}");
                    } else {
                        match read_status_line(&mut first) {
                            Ok(200) => return,
                            // The first socket was refused too: a fresh pair
                            // may still see the released slot.
                            Ok(503) => {
                                last =
                                    "first (admitted) concurrent connection: status 503".to_string()
                            }
                            Ok(status) => regression_failure(
                                sandbox,
                                "max_connections",
                                "first (admitted) concurrent connection",
                                "status 200",
                                &format!("status {status}"),
                            ),
                            Err(error) => {
                                last = format!("first (admitted) concurrent connection: {error}")
                            }
                        }
                    }
                }
                // A complete non-503 answer means the floor admitted both (or
                // answered something else): a real failure, not a retry.
                Ok(status) => regression_failure(
                    sandbox,
                    "max_connections",
                    "second concurrent connection (refused at accept, no request sent)",
                    "status 503",
                    &format!("status {status}"),
                ),
                Err(error) => {
                    last = format!(
                        "second concurrent connection (refused at accept, no request sent): {error}"
                    )
                }
            }
        }
        if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
            regression_failure(
                sandbox,
                "max_connections",
                if streams {
                    "two concurrent connections"
                } else {
                    "first (admitted) concurrent connection"
                },
                if streams {
                    "status 200 on both connections"
                } else {
                    "status 200 on the admitted connection after a 503 for the second"
                },
                &last,
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Connect one probe socket, reporting a dead listener through the config-key
/// channel instead of a bare panic (docs/testing.md §6).
fn connect_for_probe(sandbox: &mut Sandbox, key: &str, what: &str, address: &str) -> TcpStream {
    match TcpStream::connect(address) {
        Ok(stream) => stream,
        Err(error) => regression_failure(
            sandbox,
            key,
            what,
            "a connectable daemon",
            &format!("connect {address}: {error}"),
        ),
    }
}

/// One MCP request over the Streamable HTTP binding.
fn mcp_min_exchange(
    sandbox: &mut Sandbox,
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
    exchange_admitted(sandbox, address, &head, &body)
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

/// An SSE stream that has to reach the handler. `sse_open_retrying` already
/// retries the subscriber-release 429; this also retries the admission refusal
/// and reports through the config key that the caller's probe belongs to
/// instead of panicking (#155).
fn sse_open_admitted(
    sandbox: &mut Sandbox,
    key: &str,
    address: &str,
    path: &str,
    last_id: Option<u64>,
) -> TcpStream {
    let deadline = Instant::now() + ADMISSION_WAIT;
    loop {
        match sse_open_retrying(address, path, last_id) {
            Ok(stream) => return stream,
            Err(error) => {
                if Instant::now() >= deadline || !daemon_state(sandbox).starts_with("running") {
                    regression_failure(
                        sandbox,
                        key,
                        &format!("GET {path} (SSE)"),
                        "an admitted SSE stream within the admission wait",
                        &error,
                    );
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
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
    let port_before =
        fs::read_to_string(sandbox.data.join("run").join("http.port")).expect("http.port");

    // wait_for_http drops a bare TCP connection. With max_connections = 1 the
    // daemon can still count it when the first real request lands, so the first
    // probe is retried against the bounded admission wait instead of failing.
    let warmup = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        address
    );
    let probe = exchange_admitted(&mut sandbox, &address, &warmup, b"");
    if probe.status != 200 {
        regression_failure(
            &mut sandbox,
            "max_connections",
            "GET /api/v1/services (the first admitted request)",
            "status 200 within the admission wait",
            &probe.describe(),
        );
    }

    // /mcp always authenticates with the configured password.
    set_password(&sandbox, MIN_CONFIG_PASSWORD);

    // One installed plugin, so the plugin-UI surfaces (WS/SSE) exist at all.
    let installed = exchange_admitted(
        &mut sandbox,
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
    let started = exchange_admitted(
        &mut sandbox,
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
    let over_package = exchange_admitted(
        &mut sandbox,
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
    let over_json = exchange_admitted(
        &mut sandbox,
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
    let under_header = exchange_admitted(
        &mut sandbox,
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
    let over_header = exchange_admitted(
        &mut sandbox,
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
    let huge_head = request_head(
        &address,
        "GET",
        "/api/v1/services",
        "",
        0,
        &[("X-Pad", big.as_str())],
    );
    let mut huge_header = exchange(&address, &huge_head, b"");
    // A readable 503 here is the admission gate answering, not the header
    // verdict: wait for a slot instead of reading it as a regression.
    let huge_deadline = Instant::now() + ADMISSION_WAIT;
    while huge_header.status == 503
        && Instant::now() < huge_deadline
        && daemon_state(&mut sandbox).starts_with("running")
    {
        std::thread::sleep(Duration::from_millis(25));
        huge_header = exchange(&address, &huge_head, b"");
    }
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
    // Windows fallback admits before it spawns the per-connection thread.
    assert_concurrent_connections(&mut sandbox, &address, streams);

    // http_idle_timeout_secs clamps to 1 s: the Unix reactor's idle sweep and
    // the Windows fallback's per-socket read timeout both close a keep-alive
    // connection instead of idling for the compile-time default (30/60/120 s).
    // The keep-alive probe needs an admitted slot too, so retry the handshake
    // against the admission wait.
    let idle_request = format!(
        "GET /api/v1/services HTTP/1.1\r\nHost: {}\r\nConnection: keep-alive\r\n\r\n",
        address
    );
    let idle_deadline = Instant::now() + ADMISSION_WAIT;
    let (mut idle, _kept) = loop {
        let mut stream = connect_for_probe(
            &mut sandbox,
            "http_idle_timeout_secs",
            "keep-alive GET /api/v1/services",
            &address,
        );
        if let Err(error) = stream.set_read_timeout(Some(Duration::from_millis(500))) {
            regression_failure(
                &mut sandbox,
                "http_idle_timeout_secs",
                "keep-alive GET /api/v1/services",
                "a socket with a read timeout",
                &format!("read timeout: {error}"),
            );
        }
        let kept = match stream.write_all(idle_request.as_bytes()) {
            Ok(()) => read_one_response(&mut stream),
            // A refused connection can be reset on write: not admitted yet.
            Err(_) => RawResponse {
                status: 0,
                body: Vec::new(),
                headers: Vec::new(),
            },
        };
        if kept.status == 200 {
            break (stream, kept);
        }
        if kept.status != 0 && kept.status != 503 {
            // A complete answer that is neither success nor the admission
            // gate is a real failure; do not retry it away.
            regression_failure(
                &mut sandbox,
                "http_idle_timeout_secs",
                "keep-alive GET /api/v1/services",
                "status 200 on the keep-alive connection",
                &format!("status {}", kept.status),
            );
        }
        if Instant::now() >= idle_deadline || !daemon_state(&mut sandbox).starts_with("running") {
            regression_failure(
                &mut sandbox,
                "http_idle_timeout_secs",
                "keep-alive GET /api/v1/services",
                "status 200 on the keep-alive connection",
                &format!("status {}", kept.status),
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
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
    let listed = mcp_min_exchange(
        &mut sandbox,
        &address,
        MIN_CONFIG_PASSWORD,
        "tools/list",
        json!({}),
        None,
    );
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
        &mut sandbox,
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

    let initialize = mcp_min_exchange(
        &mut sandbox,
        &address,
        MIN_CONFIG_PASSWORD,
        "initialize",
        json!({}),
        None,
    );
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
        let (mut ws, status, head) = ws_handshake_admitted(&mut sandbox, &address, &ws_path, &[]);
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
        // receive the event published while no subscriber was attached. The
        // subscriber slot and the connection slot are both released by the
        // server asynchronously, so the open waits them out instead of failing.
        let mut sse = sse_open_admitted(
            &mut sandbox,
            "max_subscribers_per_plugin",
            &address,
            &events_path,
            None,
        );
        let rpc_path = format!("{}rpc", base);
        let call = br#"{"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
        let answered = exchange_admitted(
            &mut sandbox,
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
        let published = exchange_admitted(
            &mut sandbox,
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

        let mut replay = sse_open_admitted(
            &mut sandbox,
            "max_subscribers_per_plugin",
            &address,
            &events_path,
            Some(last_id),
        );
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
        // Both surfaces answer a rejection for the subscriber limit, so the
        // probe must reach the handler: an admission refusal is retried.
        let handshake = exchange_admitted(&mut sandbox, &address, &upgrade, b"");
        expect_status(
            &mut sandbox,
            "max_subscribers_per_plugin",
            "GET <base>ws (Upgrade: websocket, zero subscribers allowed)",
            &[429],
            &handshake,
        );
        let events = exchange_admitted(
            &mut sandbox,
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
    let port_after =
        fs::read_to_string(sandbox.data.join("run").join("http.port")).expect("http.port");
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

/// Open one streaming `tools/call` and return the connection. The caller reads
/// the response head first, so a 503 keeps its status without waiting for SSE.
fn mcp_stream_request(address: &str, id: u64, name: &str) -> TcpStream {
    let body = serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
                "progressToken": "t1"
            }
        }
    }))
    .expect("body");
    let mut stream = TcpStream::connect(address).expect("mcp connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: tools/call\r\nMcp-Name: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        address,
        TEST_PASSWORD,
        name,
        body.len()
    );
    stream.write_all(request.as_bytes()).expect("write head");
    stream.write_all(&body).expect("write body");
    stream
}

/// A `max_streams = 1` sandbox with the event plugin installed and started.
fn mcp_disconnect_sandbox() -> (Sandbox, String) {
    let mut sandbox = Sandbox::new();
    sandbox.allow_insecure_install_with("max_streams = 1\n");
    let key = SigningKey::from_bytes(&[62u8; 32]);
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
    set_password(&sandbox, TEST_PASSWORD);
    (sandbox, address)
}

/// Open a hung `tools/call`, leave (optionally after trailing bytes), and
/// require the long-lived slot back before the plugin call can finish.
fn assert_mcp_disconnect_releases(address: &str, id: u64, trailing: Option<u8>) {
    // The plugin sleeps for five seconds, so the stream is still open when we
    // drop the client after its head.
    let mut slow = mcp_stream_request(address, id, "demo.slow");
    assert_eq!(read_response_head(&mut slow).0, 200);
    if let Some(byte) = trailing {
        slow.write_all(&[byte]).expect("trailing byte");
    }
    let dropped_at = Instant::now();
    drop(slow);

    let mut released = false;
    // The plugin sleeps for five seconds; a slot released only when that call
    // ends would still be busy here.
    for _ in 0..200 {
        let mut candidate = mcp_stream_request(address, id + 1, "demo.status");
        let (status, _) = read_response_head(&mut candidate);
        if status == 200 {
            released = true;
            drop(candidate);
            break;
        }
        assert_eq!(status, 503, "only the busy status is retried");
        drop(candidate);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        released,
        "a disconnected MCP client must release the max_streams slot"
    );
    assert!(
        dropped_at.elapsed() < Duration::from_secs(2),
        "the slot must be released when the client leaves, not when the hung \
         plugin call finally returns (took {:?})",
        dropped_at.elapsed()
    );
}
