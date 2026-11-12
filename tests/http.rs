use std::fs;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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
        fs::copy(binary, &svcd).expect("copy svcd");
        fs::copy(binary, &svc).expect("copy svc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&svcd, fs::Permissions::from_mode(0o755)).expect("chmod");
            fs::set_permissions(&svc, fs::Permissions::from_mode(0o755)).expect("chmod");
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
    let mut stream = TcpStream::connect(address).expect("connect");
    let mut request = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        method,
        path,
        address,
        body.len()
    );
    if !content_type.is_empty() {
        request.push_str(&format!("Content-Type: {}\r\n", content_type));
    }
    for (name, value) in extra_headers {
        request.push_str(&format!("{}: {}\r\n", name, value));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("write headers");
    stream.write_all(body).expect("write body");
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
    let body = serde_json::from_slice(&response[split + 4..]).expect("json body");
    Response {
        status,
        body,
        headers,
    }
}

fn header_values<'a>(response: &'a Response, name: &str) -> Vec<&'a str> {
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

fn plugin_package(key: &SigningKey) -> Vec<u8> {
    let (name, data) = if cfg!(windows) {
        (
            "agent.cmd",
            b"@echo off\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\n".to_vec(),
        )
    } else {
        (
            "agent.sh",
            b"#!/bin/sh\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n".to_vec(),
        )
    };
    let path = format!("payload/bin/{}", name);
    let manifest = json!({
        "schema": 1,
        "name": "demo",
        "version": "1.0.0",
        "desc": "http plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": path, "args": [], "ready": "notify"},
        "files": [{"path": path, "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
    });
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
    tar.extend_from_slice(&header(&path, data.len(), 0o755));
    tar.extend_from_slice(&data);
    tar.extend(std::iter::repeat_n(0, padding(data.len())));
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
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
