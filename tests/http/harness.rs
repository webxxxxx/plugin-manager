//! Shared HTTP integration-test harness.

use super::support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

pub(crate) struct Sandbox {
    dirs: support::SandboxDirs,
    pub(crate) daemon: Option<Child>,
    /// Overridable so the bounded-wait test does not spend the production bound.
    pub(crate) stop_timeout: Duration,
}

impl std::ops::Deref for Sandbox {
    type Target = support::SandboxDirs;

    fn deref(&self) -> &Self::Target {
        &self.dirs
    }
}

impl std::ops::DerefMut for Sandbox {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.dirs
    }
}

impl Sandbox {
    pub(crate) fn new() -> Self {
        Sandbox {
            dirs: support::sandbox_dirs("http"),
            daemon: None,
            stop_timeout: support::STOP_TIMEOUT,
        }
    }

    pub(crate) fn env(&self, command: &mut Command) {
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
    pub(crate) fn allow_insecure_install(&self) {
        self.allow_insecure_install_with("");
    }

    /// Same, plus extra `svcd.conf` keys for one test.
    pub(crate) fn allow_insecure_install_with(&self, extra: &str) {
        fs::write(
            self.config.join("svcd.conf"),
            format!("allow_insecure_install = true\n{}", extra),
        )
        .expect("write svcd.conf");
    }

    /// Send one modern JSON-RPC request to a fresh `mcp` stdio bridge and
    /// return its first response line. One bridge per client matches the
    /// production process model.
    pub(crate) fn mcp_stdio(&self, request: Value) -> Value {
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

    pub(crate) fn run_svc(&self, args: &[&str]) -> std::process::Output {
        let mut command = Command::new(&self.svc);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svc")
    }

    pub(crate) fn start_daemon(&mut self) {
        self.start_daemon_with_bind("127.0.0.1:0");
    }

    pub(crate) fn start_daemon_with_bind(&mut self, bind: &str) {
        self.start_daemon_with_env(bind, &[]);
    }

    /// A daemon that reports the unprivileged (user) role, which is what the
    /// restart and upgrade policies key on. The default sandbox env forces the
    /// service role.
    pub(crate) fn start_daemon_as_user(&mut self) {
        self.start_daemon_with_env("127.0.0.1:0", &[("SVCD_TEST_PRIVILEGE", "unprivileged")]);
    }

    /// Start the daemon from an arbitrary binary path. The upgrade tests run it
    /// from the install tree so a Windows self-replace renames the *running*
    /// image, which is the path that has to work.
    pub(crate) fn start_daemon_from(&mut self, binary: &std::path::Path, extra: &[(&str, &str)]) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("daemon stderr");
        let mut command = Command::new(binary);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        self.env(&mut command);
        command.env("SVCD_BIND", "127.0.0.1:0");
        for (key, value) in extra {
            command.env(key, value);
        }
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    /// `start_daemon_with_bind` plus extra environment. The restart tests pin the
    /// privilege role here: the default sandbox env forces `SVCD_TEST_UNPRIVILEGED`,
    /// which makes the daemon look like a system service to the restart policy.
    pub(crate) fn start_daemon_with_env(&mut self, bind: &str, extra: &[(&str, &str)]) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("daemon stderr");
        let mut command = Command::new(&self.svcd);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        self.env(&mut command);
        command.env("SVCD_BIND", bind);
        for (key, value) in extra {
            command.env(key, value);
        }
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    /// Like `start_daemon`, but leaves the bind address to the config instead
    /// of forcing an ephemeral port through the environment.
    pub(crate) fn start_daemon_without_http_port(&mut self) {
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

    pub(crate) fn start_daemon_with_conf(&mut self, conf: &Path) {
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

    pub(crate) fn wait_for_http(&self) -> String {
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

    pub(crate) fn daemon_log(&self) -> String {
        fs::read_to_string(&self.daemon_stderr).unwrap_or_default()
    }

    /// Stop the daemon with a bounded request and a bounded wait; see
    /// [`support::stop_daemon`]. `child.wait()` alone could hang the suite
    /// when the request never reached the daemon.
    pub(crate) fn stop_daemon(&mut self) -> Result<(), String> {
        let config = self.config.clone();
        self.stop_daemon_at(&config)
    }

    /// Stop, but target `config` instead of the sandbox's own. The control
    /// socket (Unix) and the pipe name (Windows) both derive from this string,
    /// so a dir no daemon bound makes the request genuinely unreachable.
    pub(crate) fn stop_daemon_at(&mut self, config: &Path) -> Result<(), String> {
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

    pub(crate) fn shutdown(&mut self) {
        support::report_stop(self.stop_daemon());
    }

    pub(crate) fn write_core(&self) {
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

pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Value,
    pub(crate) headers: Vec<(String, String)>,
}

pub(crate) struct RawResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    pub(crate) headers: Vec<(String, String)>,
}

pub(crate) fn request(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> Response {
    request_with(address, method, path, content_type, body, &[])
}

pub(crate) fn request_with(
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

pub(crate) fn raw_request(
    address: &str,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> RawResponse {
    raw_request_with(address, method, path, content_type, body, &[])
}

pub(crate) fn request_head(
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

pub(crate) fn parse_raw_response(response: Vec<u8>) -> RawResponse {
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

pub(crate) fn read_raw_response(stream: &mut TcpStream) -> RawResponse {
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    parse_raw_response(response)
}

/// Send one hand-built request head with no body and read the full response.
/// Used when the test must declare a framing the helpers cannot express; the
/// bounded read reports the daemon log on failure, per the suite's convention.
pub(crate) fn send_request_head(sandbox: &Sandbox, address: &str, head: &str) -> RawResponse {
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

pub(crate) fn read_one_response(stream: &mut TcpStream) -> RawResponse {
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

pub(crate) fn raw_request_with(
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
pub(crate) fn raw_request_split_body(
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

pub(crate) fn raw_request_paced(
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
pub(crate) fn raw_request_chunked(
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

pub(crate) fn multipart_body(boundary: &str, filename: &str, content: &[u8]) -> Vec<u8> {
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

pub(crate) fn decode_chunked(body: &[u8]) -> Vec<u8> {
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

pub(crate) fn json_body(response: &RawResponse) -> Value {
    serde_json::from_slice(&response.body).expect("json body")
}

pub(crate) fn header_values<'a>(response: &'a Response, name: &str) -> Vec<&'a str> {
    response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

pub(crate) fn raw_header_values<'a>(response: &'a RawResponse, name: &str) -> Vec<&'a str> {
    response
        .headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

pub(crate) fn cookie_value(response: &Response, name: &str) -> Option<String> {
    header_values(response, "set-cookie")
        .into_iter()
        .find_map(|value| {
            let (pair, _) = value.split_once(';')?;
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_string())
        })
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
pub(crate) const ADMISSION_WAIT: Duration = Duration::from_secs(30);

pub(crate) fn daemon_state(sandbox: &mut Sandbox) -> String {
    match sandbox.daemon.as_mut() {
        None => "no daemon handle".to_string(),
        Some(child) => match child.try_wait() {
            Ok(None) => format!("running (pid {})", child.id()),
            Ok(Some(status)) => format!("EXITED ({status})"),
            Err(error) => format!("try_wait failed: {error}"),
        },
    }
}

pub(crate) fn regression_failure(
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

/// Read exactly one response head. A HEAD response must put the next response
/// immediately after this terminator, with no body bytes in between.
pub(crate) fn read_response_head_raw(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).expect("response head");
        head.push(byte[0]);
    }
    head
}

pub(crate) fn parse_response_head(head: &[u8]) -> (u16, Vec<(String, String)>) {
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

pub(crate) fn read_response_head(stream: &mut TcpStream) -> (u16, Vec<(String, String)>) {
    parse_response_head(&read_response_head_raw(stream))
}

pub(crate) fn tail_text(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
