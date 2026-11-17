use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// How long a sandbox waits for its daemon to exit after a shutdown request.
/// An unreachable request (the socket is not up yet, or a test panicked before
/// the daemon got going) must not hang the suite: past this bound the daemon is
/// killed and the wait reports what failed.
const DAEMON_STOP_TIMEOUT: Duration = Duration::from_secs(10);

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
    plugin: PathBuf,
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
            "svcd-plugin-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let config = root.join("config");
        let data = root.join("data");
        let logs = root.join("logs");
        let trust = config.join("trust.d");
        fs::create_dir_all(&trust).expect("create sandbox");
        fs::create_dir_all(&data).expect("create data");
        fs::create_dir_all(&logs).expect("create logs");

        let binary = env!("CARGO_BIN_EXE_svcd");
        let names = if cfg!(windows) {
            ("svcd.exe", "svc.exe", "plugin.exe")
        } else {
            ("svcd", "svc", "plugin")
        };
        let svcd = root.join(names.0);
        let svc = root.join(names.1);
        let plugin = root.join(names.2);
        for path in [&svcd, &svc, &plugin] {
            // The multicall dispatch only reads argv[0]'s file name, so a link
            // is enough on Unix. A copy that is exec'd right after it is
            // written races its own close on slow filesystems (/mnt/d, DrvFs)
            // and fails with ETXTBSY; the link has no such window. Windows
            // keeps the copy because creating symlinks needs a privilege.
            //
            // A link makes `current_exe()` resolve to the cargo-built binary
            // instead of the sandbox entry. The plugin suite never self-installs
            // (that path lives in install.rs and is exercised by the suites that
            // keep their own copies), so only argv[0] dispatch is under test.
            #[cfg(unix)]
            std::os::unix::fs::symlink(binary, path).expect("link multicall");
            #[cfg(not(unix))]
            fs::copy(binary, path).expect("copy multicall");
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
            plugin,
            daemon: None,
            stop_timeout: DAEMON_STOP_TIMEOUT,
        }
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TRUST_DIR", &self.trust)
            .env("SVCD_BOOT_SCAN_WAIT_MS", "0")
            .env("SVCD_RESCAN_INTERVAL_MS", "0");
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

    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        self.env(&mut command);
        command
    }

    fn try_run(&self, binary: &Path, args: &[&str]) -> std::io::Result<Output> {
        self.command(binary).args(args).output()
    }

    /// Run a CLI with a bounded wait. `output()` has none: a control request
    /// waits for a reply, and a daemon that accepts but never answers would
    /// block the suite the same way the unbounded `child.wait()` did.
    fn run_bounded(
        &self,
        binary: &Path,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output, String> {
        let mut child = self
            .command(binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot run {} {args:?}: {error}", binary.display()))?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    return child.wait_with_output().map_err(|error| {
                        format!("cannot read {} output: {error}", binary.display())
                    })
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("cannot wait for {}: {error}", binary.display()));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} {args:?} did not finish within {timeout:?}",
                    binary.display()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run(&self, binary: &Path, args: &[&str]) -> Output {
        self.try_run(binary, args).unwrap_or_else(|error| {
            panic!("run {} {args:?}: {error}", binary.display());
        })
    }

    fn run_plugin(&self, args: &[&str]) -> Output {
        self.run(&self.plugin, args)
    }

    fn run_plugin_in(&self, directory: &Path, args: &[&str]) -> Output {
        self.command(&self.plugin)
            .args(args)
            .current_dir(directory)
            .output()
            .unwrap_or_else(|error| {
                panic!(
                    "run {} {args:?} in {}: {error}",
                    self.plugin.display(),
                    directory.display()
                );
            })
    }

    fn run_svc(&self, args: &[&str]) -> Output {
        self.run(&self.svc, args)
    }

    fn wait_for_daemon(&self) {
        for _ in 0..200 {
            let output = self.run_plugin(&["list"]);
            if output.status.code() != Some(2) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("daemon never became reachable: {}", self.daemon_log());
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(&self.daemon_stderr).unwrap_or_default()
    }

    /// Stop the daemon with a bounded wait. Mirrors the diagnostic contract of
    /// `tests/http.rs`'s wait helper (a timeout carries the daemon log) but
    /// returns the message instead of panicking: `Drop` may be running while
    /// another panic unwinds, and panicking there aborts the process.
    fn stop_daemon(&mut self) -> Result<(), String> {
        let Some(mut child) = self.daemon.take() else {
            return Ok(());
        };
        // The request is bounded too: `svcd shutdown` waits for a control reply
        // with no timeout of its own.
        let request = self.run_bounded(&self.svcd, &["shutdown"], self.stop_timeout);
        let deadline = Instant::now() + self.stop_timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    // The daemon is gone. A failed request here is a race with
                    // the exit (the reply can be lost on the way out), not a
                    // test failure: keep it visible, do not fail the test.
                    if !matches!(&request, Ok(output) if output.status.success()) {
                        eprintln!(
                            "sandbox: the shutdown request reported {} but the daemon exited",
                            failed_request(&request)
                        );
                    }
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => {
                    // The handle is already out of `self.daemon`, so leaving the
                    // process running here would leak it past the Drop.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "cannot wait for the daemon: {error}; killed it. shutdown request: {}; daemon stderr (tail):\n{}",
                        failed_request(&request),
                        tail(&self.daemon_log(), 20)
                    ));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the daemon did not stop within {:?}; killed it. shutdown request: {}; daemon stderr (tail):\n{}",
                    self.stop_timeout,
                    failed_request(&request),
                    tail(&self.daemon_log(), 20)
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn shutdown(&mut self) {
        match self.stop_daemon() {
            Ok(()) => {}
            // A panic while another panic unwinds aborts the process, which
            // would hide the failure that started it: print instead.
            Err(message) if std::thread::panicking() => eprintln!("sandbox: {message}"),
            Err(message) => panic!("sandbox: {message}"),
        }
    }

    fn write_key(&self, key: &SigningKey) {
        fs::write(
            self.trust.join("test.pub"),
            pem_from_public(&key.verifying_key().to_bytes()),
        )
        .expect("write trust key");
    }

    fn write_package(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, bytes).expect("write spk");
        path
    }

    fn write_core(&self, id: &str) {
        let directory = self.config.join("services.d");
        fs::create_dir_all(&directory).expect("services.d");
        let (path, args) = if cfg!(windows) {
            (r"C:\Windows\System32\cmd.exe", vec!["/c", "exit", "0"])
        } else {
            ("/bin/true", Vec::new())
        };
        let definition = json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "core",
            "exec": {"path": path, "args": args, "ready": "exec"}
        });
        fs::write(
            directory.join(format!("{}.json", id)),
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

fn other_target() -> &'static str {
    if target() == "aarch64-unknown-linux-musl" {
        "x86_64-unknown-linux-musl"
    } else {
        "aarch64-unknown-linux-musl"
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

fn package(key: &SigningKey, version: &str) -> Vec<u8> {
    package_config(key, version, b"{}")
}

fn package_config(key: &SigningKey, version: &str, config: &[u8]) -> Vec<u8> {
    let (name, data) = script();
    let path = format!("payload/bin/{}", name);
    let config = config.to_vec();
    let files = vec![
        json!({
            "path": path,
            "size": data.len(),
            "sha256": hex_sha256(&data),
            "mode": 0o755
        }),
        json!({
            "path": "config/config",
            "size": config.len(),
            "sha256": hex_sha256(&config),
            "mode": 0o644
        }),
    ];
    let manifest = json!({
        "schema": 1,
        "name": "demo",
        "version": version,
        "desc": "test plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": path, "args": [], "ready": "notify"},
        "config": "config/config",
        "files": files
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
    for (path, data, mode) in [
        (path.as_str(), data.as_slice(), 0o755),
        ("config/config", config.as_slice(), 0o644),
    ] {
        tar.extend_from_slice(&header(path, data.len(), mode));
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

fn package_with_manifest(
    key: &SigningKey,
    manifest: &Value,
    files: &[(&str, &[u8], u32)],
) -> Vec<u8> {
    let manifest = serde_json::to_vec(manifest).expect("manifest");
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

fn clock_script() -> (&'static str, Vec<u8>) {
    if cfg!(windows) {
        (
            "payload/bin/clock.cmd",
            b"@echo off\r\nif /I \"%~1\"==\"stop\" (\r\n  echo stopped>\"%SVCD_DATA_DIR%\\clock.stopped\"\r\n  exit /b 0\r\n)\r\nif /I \"%~1\"==\"status\" (\r\n  echo clock ok\r\n  exit /b 0\r\n)\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n"
                .to_vec(),
        )
    } else {
        (
            "payload/bin/clock.sh",
            b"#!/bin/sh\ncase \"$1\" in\n  stop)\n    echo stopped > \"$SVCD_DATA_DIR/clock.stopped\"\n    ;;\n  status)\n    echo clock ok\n    ;;\n  *)\n    while :; do sleep 1; done\n    ;;\nesac\n"
                .to_vec(),
        )
    }
}

fn clock_manifest(exec: &str, data: &[u8]) -> Value {
    let interpreter = if cfg!(windows) {
        json!({"name": "cmd", "args": ["/C"]})
    } else {
        json!({"name": "sh"})
    };
    json!({
        "schema": 2,
        "name": "clock",
        "version": "1.0.0",
        "desc": "clock service",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec, "ready": "exec"},
        "interpreter": interpreter,
        "lifecycle": {
            "start": {"path": exec, "args": ["start"]},
            "stop": {"path": exec, "args": ["stop"]},
            "status": {"path": exec, "args": ["status"]}
        },
        "files": [{
            "path": exec,
            "size": data.len(),
            "sha256": hex_sha256(data),
            "mode": 0o755
        }]
    })
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

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

fn pem_from_private(key: &SigningKey) -> Vec<u8> {
    const PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(&key.to_bytes());
    let body = BASE64.encode(der);
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in body.as_bytes().chunks(48) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    pem.into_bytes()
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

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("json status")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// What the shutdown request looked like, for diagnostics.
fn failed_request(outcome: &Result<Output, String>) -> String {
    match outcome {
        Ok(output) => format!(
            "exit {:?} stderr {:?}",
            output.status.code(),
            stderr(output).trim()
        ),
        Err(message) => message.clone(),
    }
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// A shutdown request that never reaches the daemon used to block the suite
/// forever in `child.wait()` (the failure inside `Sandbox::drop` after a
/// panicking test). The wait is bounded now and names what failed.
#[test]
fn shutdown_bounds_its_wait_when_the_daemon_cannot_be_reached() {
    let mut sandbox = Sandbox::new();
    sandbox.stop_timeout = Duration::from_secs(1);
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    // Move the config directory away: the CLI can no longer resolve the control
    // channel (a socket on Unix, a named pipe on Windows), so the request fails
    // the way it does when a test panics before the daemon is up.
    let moved = sandbox.root.join("config-moved");
    fs::rename(&sandbox.config, &moved).expect("move the config directory");

    let started = Instant::now();
    let error = sandbox
        .stop_daemon()
        .expect_err("a daemon that had to be killed is reported, not ignored");

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait must be bounded, took {:?}",
        started.elapsed()
    );
    assert!(
        error.contains("did not stop") && error.contains("shutdown request:"),
        "the error must name the failed request: {error}"
    );
    assert!(sandbox.daemon.is_none(), "the handle must be consumed");
}

/// The failure that started the hang: a panicking test drops its sandbox while
/// unwinding. The Drop must stay bounded and must not panic again (a panic in
/// a destructor during unwind aborts the process and hides the real failure).
#[test]
fn dropping_a_sandbox_while_unwinding_stays_bounded() {
    let mut sandbox = Sandbox::new();
    sandbox.stop_timeout = Duration::from_secs(1);
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let moved = sandbox.root.join("config-moved");
    fs::rename(&sandbox.config, &moved).expect("move the config directory");

    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        // Own the sandbox in this frame so its Drop runs while unwinding.
        let _sandbox = sandbox;
        panic!("simulated test failure");
    }));

    assert!(outcome.is_err(), "the simulated failure must unwind");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "dropping the sandbox while unwinding must stay bounded, took {:?}",
        started.elapsed()
    );
}

#[test]
fn installs_lists_starts_and_uninstalls_a_plugin() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[9u8; 32]);
    sandbox.write_key(&key);
    let package = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let installed = sandbox.run_plugin(&["install", package.to_str().expect("utf8")]);
    assert_eq!(
        installed.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&installed)
    );
    assert!(String::from_utf8_lossy(&installed.stdout).contains("demo"));
    let plugin_dir = sandbox.data.join("plugins").join("demo");
    assert_eq!(
        fs::read(plugin_dir.join("config")).expect("config"),
        b"{}".to_vec()
    );

    let listed = sandbox.run_plugin(&["list"]);
    let text = String::from_utf8_lossy(&listed.stdout);
    assert!(text.contains("demo"), "{}", text);
    assert!(text.contains("plugin"), "{}", text);

    let svc_list = sandbox.run_svc(&["list"]);
    assert!(
        String::from_utf8_lossy(&svc_list.stdout).contains("demo"),
        "{}",
        String::from_utf8_lossy(&svc_list.stdout)
    );

    let status = sandbox.run_plugin(&["status", "demo", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    assert_eq!(json(&status)["summary"]["kind"], "plugin");

    let started = sandbox.run_plugin(&["start", "demo"]);
    assert_eq!(started.status.code(), Some(0), "{}", stderr(&started));
    let running = sandbox.run_plugin(&["status", "demo", "--json"]);
    assert_eq!(json(&running)["summary"]["state"], "running");

    let stopped = sandbox.run_plugin(&["stop", "demo"]);
    assert_eq!(stopped.status.code(), Some(0), "{}", stderr(&stopped));
    let stopped_status = sandbox.run_plugin(&["status", "demo", "--json"]);
    assert_eq!(json(&stopped_status)["summary"]["state"], "stopped");

    sandbox.shutdown();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let after_restart = sandbox.run_plugin(&["list"]);
    assert!(
        String::from_utf8_lossy(&after_restart.stdout).contains("demo"),
        "plugin did not survive daemon restart: {}",
        String::from_utf8_lossy(&after_restart.stdout)
    );

    let core = sandbox.run_plugin(&["status", "svc"]);
    assert_eq!(core.status.code(), Some(1));
    assert!(stderr(&core).contains("not a plugin"));

    let start_core = sandbox.run_plugin(&["start", "svc"]);
    assert_eq!(start_core.status.code(), Some(1));
    assert!(stderr(&start_core).contains("not a plugin"));

    assert_eq!(
        sandbox.run_plugin(&["start", "demo"]).status.code(),
        Some(0)
    );
    let removed = sandbox.run_plugin(&["uninstall", "demo"]);
    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!sandbox
        .run_plugin(&["list"])
        .stdout
        .windows(4)
        .any(|w| w == b"demo"));
    assert!(
        plugin_dir.join("config").is_file(),
        "uninstall without --purge must keep config"
    );

    let reinstalled = sandbox.run_plugin(&["install", package.to_str().expect("utf8")]);
    assert_eq!(
        reinstalled.status.code(),
        Some(0),
        "{}",
        stderr(&reinstalled)
    );
    let purged = sandbox.run_plugin(&["uninstall", "demo", "--purge"]);
    assert_eq!(purged.status.code(), Some(0), "{}", stderr(&purged));
    assert!(!plugin_dir.exists(), "--purge must remove the plugin tree");
}

#[test]
fn keep_old_and_replace_have_the_documented_layouts() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[10u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    let v2 = sandbox.write_package("demo-1.1.0.spk", &package(&key, "1.1.0"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        sandbox
            .run_plugin(&[
                "install",
                v2.to_str().expect("utf8"),
                "--strategy",
                "keep-old"
            ])
            .status
            .code(),
        Some(0)
    );
    let plugin = sandbox.data.join("plugins").join("demo");
    assert!(plugin.join("1.0.0").is_dir());
    assert!(plugin.join("1.1.0").is_dir());
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.1.0"
    );

    let replace = sandbox.run_plugin(&[
        "install",
        v2.to_str().expect("utf8"),
        "--strategy",
        "replace",
    ]);
    assert_eq!(replace.status.code(), Some(0), "{}", stderr(&replace));
    assert!(!plugin.join("1.0.0").is_dir());
    assert!(plugin.join("1.1.0").is_dir());
}

#[test]
fn a_failed_replace_restores_the_old_version() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[17u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package_config(&key, "1.0.0", b"one"));
    let v2 = sandbox.write_package("demo-1.1.0.spk", &package_config(&key, "1.1.0", b"two"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );

    let plugin = sandbox.data.join("plugins").join("demo");
    let hash = plugin.join("config.template.sha256");
    assert!(hash.is_file(), "v1 should write the template hash");
    // Force copy_config to fail after the new version has been written and
    // current has switched: the hash path now cannot be written as a file.
    fs::remove_file(&hash).expect("remove hash");
    fs::create_dir_all(&hash).expect("create hash directory");

    let failed = sandbox.run_plugin(&[
        "install",
        v2.to_str().expect("utf8"),
        "--strategy",
        "replace",
    ]);
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));

    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.0.0"
    );
    assert!(
        plugin.join("1.0.0").is_dir(),
        "old version was not restored"
    );
    assert!(
        !plugin.join("1.1.0").exists(),
        "failed new version was not removed"
    );
    assert_eq!(fs::read(plugin.join("config")).expect("config"), b"one");
    assert!(
        !fs::read_dir(&plugin)
            .expect("plugin dir")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".backup-")),
        "failed replace left a backup directory"
    );
}

#[test]
fn default_install_strategy_is_replace() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[24u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    let v2 = sandbox.write_package("demo-1.1.0.spk", &package(&key, "1.1.0"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        sandbox
            .run_plugin(&["install", v2.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    let plugin = sandbox.data.join("plugins").join("demo");
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.1.0"
    );
    assert!(plugin.join("1.1.0").is_dir());
    assert!(
        !plugin.join("1.0.0").exists(),
        "the default install must replace the old version"
    );
}

#[test]
fn an_invalid_package_leaves_the_installed_version_alone() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[11u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );

    let mut bad = package(&key, "1.1.0");
    let needle = b"\"demo\"";
    let position = bad
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("name");
    bad[position + 1] = b'D';
    let bad = sandbox.write_package("bad-1.1.0.spk", &bad);
    let failed = sandbox.run_plugin(&["install", bad.to_str().expect("utf8")]);
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));

    let plugin = sandbox.data.join("plugins").join("demo");
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.0.0"
    );
    assert!(plugin.join("1.0.0").is_dir());
    assert!(!plugin.join("1.1.0").exists());
}

#[test]
fn rejects_a_plugin_id_that_belongs_to_a_core_service() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[13u8; 32]);
    sandbox.write_key(&key);
    sandbox.write_core("demo");
    let package = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let output = sandbox.run_plugin(&["install", package.to_str().expect("utf8")]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("core service") || stderr(&output).contains("conflicts"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn running_upgrade_keeps_old_until_restart_and_replace_stops_first() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[12u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    let v2 = sandbox.write_package("demo-1.1.0.spk", &package(&key, "1.1.0"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        sandbox.run_plugin(&["start", "demo"]).status.code(),
        Some(0)
    );

    assert_eq!(
        sandbox
            .run_plugin(&[
                "install",
                v2.to_str().expect("utf8"),
                "--strategy",
                "keep-old"
            ])
            .status
            .code(),
        Some(0)
    );
    let plugin = sandbox.data.join("plugins").join("demo");
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.1.0"
    );
    let running = sandbox.run_plugin(&["status", "demo", "--json"]);
    assert_eq!(json(&running)["summary"]["state"], "running");
    assert_eq!(json(&running)["summary"]["version"], "1.1.0");
    assert!(plugin.join("1.0.0").is_dir());

    assert_eq!(sandbox.run_plugin(&["stop", "demo"]).status.code(), Some(0));
    assert_eq!(
        sandbox.run_plugin(&["start", "demo"]).status.code(),
        Some(0)
    );

    let replace = sandbox.run_plugin(&[
        "install",
        v2.to_str().expect("utf8"),
        "--strategy",
        "replace",
    ]);
    assert_eq!(replace.status.code(), Some(0), "{}", stderr(&replace));
    assert!(!plugin.join("1.0.0").is_dir());
    let stopped = sandbox.run_plugin(&["status", "demo", "--json"]);
    assert_eq!(json(&stopped)["summary"]["state"], "stopped");
}

#[test]
fn config_flow_copies_prints_exports_and_preserves_on_upgrade() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[14u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package_config(&key, "1.0.0", b"one"));
    let v2 = sandbox.write_package("demo-1.1.0.spk", &package_config(&key, "1.1.0", b"two"));
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    let plugin = sandbox.data.join("plugins").join("demo");
    assert_eq!(fs::read(plugin.join("config")).expect("config"), b"one");

    let printed = sandbox.run_plugin(&["editconfig", "demo", "--print"]);
    assert_eq!(printed.status.code(), Some(0), "{}", stderr(&printed));
    assert_eq!(&printed.stdout, b"one");

    let config_out = sandbox.root.join("demo.config");
    let exported = sandbox.run_plugin(&[
        "export",
        "demo",
        "--config-only",
        "--out",
        config_out.to_str().expect("utf8"),
    ]);
    assert_eq!(exported.status.code(), Some(0), "{}", stderr(&exported));
    assert_eq!(fs::read(&config_out).expect("config export"), b"one");

    let full_out = sandbox.root.join("demo.spkb");
    let full = sandbox.run_plugin(&["export", "demo", "--out", full_out.to_str().expect("utf8")]);
    assert_eq!(full.status.code(), Some(0), "{}", stderr(&full));
    let names = tar_names(&fs::read(&full_out).expect("spkb"));
    assert_eq!(names.first().map(String::as_str), Some("backup.json"));
    assert!(names.iter().any(|name| name == "plugin.spk"), "{names:?}");
    assert!(names.iter().any(|name| name == "config"), "{names:?}");

    assert_eq!(
        sandbox
            .run_plugin(&["install", v2.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(fs::read(plugin.join("config")).expect("config"), b"one");
    assert_eq!(
        fs::read(plugin.join("config.new")).expect("config.new"),
        b"two"
    );

    let imported = sandbox.run_plugin(&["import", full_out.to_str().expect("utf8"), "--no-config"]);
    assert_eq!(imported.status.code(), Some(0), "{}", stderr(&imported));
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.0.0"
    );

    let core = sandbox.run_plugin(&["export", "svc"]);
    assert_eq!(core.status.code(), Some(1));

    let spk = sandbox.run_plugin(&["import", v1.to_str().expect("utf8"), "--force"]);
    assert_eq!(spk.status.code(), Some(0), "{}", stderr(&spk));
}

#[test]
fn bundles_a_directory_into_a_working_signed_package() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[15u8; 32]);
    sandbox.write_key(&key);
    let source = sandbox.root.join("demo");
    let (script_name, data) = script();
    let exec = source.join("payload/bin").join(script_name);
    fs::create_dir_all(exec.parent().expect("exec parent")).expect("source");
    fs::write(&exec, &data).expect("exec");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let key_path = sandbox.root.join("bundle.key");
    fs::write(&key_path, pem_from_private(&key)).expect("key");

    let output = sandbox.run_plugin_in(
        &sandbox.root,
        &[
            "bundle",
            "demo",
            "--key",
            key_path.to_str().expect("key path"),
            "--key-id",
            "test",
            "--version",
            "1.0.0",
            "--desc",
            "bundled demo",
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // The default artifact name carries the entire target triple, not just the
    // arch: x86_64 is shared by the musl and gnu packages.
    let package_name = format!("demo-1.0.0-{}.spk", target());
    let package = sandbox.root.join(&package_name);
    assert!(package.is_file(), "missing {}", package.display());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&package_name),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let installed = sandbox.run_plugin(&["install", package.to_str().expect("package")]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));
    assert!(sandbox
        .data
        .join("plugins")
        .join("demo")
        .join("1.0.0")
        .join("payload/bin")
        .join(script_name)
        .is_file());
}

#[test]
fn verify_reports_a_signed_package_without_installing_it() {
    let sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[17u8; 32]);
    sandbox.write_key(&key);
    let bytes = package(&key, "1.0.0");
    let package = sandbox.write_package("demo-1.0.0.spk", &bytes);

    let output = sandbox.run_plugin(&["verify", package.to_str().expect("package"), "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("verify json");
    assert_eq!(value["ok"], true);
    assert_eq!(value["file"], "demo-1.0.0.spk");
    assert_eq!(value["name"], "demo");
    assert_eq!(value["version"], "1.0.0");
    assert_eq!(value["platform"], target());
    assert_eq!(value["schema"], 1u64);
    assert_eq!(value["size"], bytes.len() as u64);
    assert_eq!(value["sha256"], hex_sha256(&bytes));
    assert_eq!(value["capabilities"], json!([]));
    assert_eq!(value["compat"], Value::Null);
    assert_eq!(value["signature"]["state"], "valid");
    assert_eq!(value["signature"]["keyId"], "test");
    assert_eq!(value["signature"]["trusted"], true);
    assert_eq!(value["host"]["matchesHost"], true);
    assert_eq!(value["host"]["device"], Value::Null);
    assert_eq!(value["host"]["interpreter"], Value::Null);
    assert_eq!(value["files"], 2u64);
    assert!(
        !sandbox.data.join("plugins").join("demo").exists(),
        "verify must not install the package"
    );
}

#[test]
fn verify_reports_capabilities_and_compat() {
    let sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[19u8; 32]);
    sandbox.write_key(&key);
    fs::write(sandbox.config.join("compat.conf"), "device = sr6110\n").expect("marker");

    let (name, data) = script();
    let path = format!("payload/bin/{}", name);
    let files = [(path.as_str(), data.as_slice(), 0o755u32)];
    let interpreter = if cfg!(windows) {
        json!({"name": "cmd", "args": ["/C"]})
    } else {
        json!({"name": "sh"})
    };
    let manifest = json!({
        "schema": 3,
        "name": "caps",
        "version": "1.0.0",
        "desc": "capability package",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": path.clone(), "args": [], "ready": "notify"},
        "interpreter": interpreter,
        "capabilities": [
            {"name": "status", "desc": "status", "inputSchema": {"type": "object"}},
            {"name": "up", "desc": "up", "inputSchema": {"type": "object"}}
        ],
        "compat": {"devices": ["sr6110"]},
        "files": [{
            "path": path,
            "size": data.len(),
            "sha256": hex_sha256(&data),
            "mode": 0o755
        }]
    });
    let bytes = package_with_manifest(&key, &manifest, &files);
    let package = sandbox.write_package("caps-1.0.0.spk", &bytes);

    let output = sandbox.run_plugin(&["verify", package.to_str().expect("package"), "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("verify json");
    assert_eq!(value["capabilities"], json!(["status", "up"]));
    assert_eq!(value["compat"], json!(["sr6110"]));
    assert_eq!(value["schema"], 3u64);
    assert_eq!(value["signature"]["trusted"], true);
    assert_eq!(value["host"]["matchesHost"], true);
    assert_eq!(value["host"]["device"], "sr6110");
    assert!(
        value["host"]["interpreter"].as_str().is_some(),
        "declared interpreter should resolve: {value}"
    );
}

#[test]
fn verify_separates_untrusted_keys_from_a_tampered_package() {
    let sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[18u8; 32]);
    let mut bytes = package(&key, "1.0.0");
    let package = sandbox.write_package("demo-1.0.0.spk", &bytes);

    let unknown = sandbox.run_plugin(&["verify", package.to_str().expect("package"), "--json"]);
    assert_eq!(unknown.status.code(), Some(0), "{}", stderr(&unknown));
    let value: Value = serde_json::from_slice(&unknown.stdout).expect("verify json");
    assert_eq!(value["ok"], true);
    // No key in trust.d means nothing was verified; claiming `valid` here would
    // let a gate publish a package whose signature was never checked.
    assert_eq!(value["signature"]["state"], "unknown-key");
    assert_eq!(value["signature"]["keyId"], "test");
    assert_eq!(value["signature"]["trusted"], false);

    sandbox.write_key(&key);
    // A valid JSON edit with the wrong bytes: the manifest signature no longer
    // matches, even though the package still parses.
    let marker = b"test plugin";
    let offset = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("manifest desc");
    bytes[offset] = b'u';
    let tampered = sandbox.write_package("tampered-1.0.0.spk", &bytes);
    let output = sandbox.run_plugin(&["verify", tampered.to_str().expect("package"), "--json"]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("verify failure json");
    assert_eq!(value["ok"], false);
    assert_eq!(value["file"], "tampered-1.0.0.spk");
    assert!(
        value["error"]
            .as_str()
            .expect("error string")
            .contains("signature is invalid"),
        "{value}"
    );
}

#[test]
fn verify_checks_the_signature_against_an_explicit_public_key() {
    let sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[23u8; 32]);
    let mut bytes = package(&key, "1.0.0");
    let package = sandbox.write_package("demo-1.0.0.spk", &bytes);
    let pubkey = sandbox.root.join("explicit.pub");
    fs::write(&pubkey, pem_from_public(&key.verifying_key().to_bytes())).expect("write pubkey");

    // The key is not in trust.d, so `trusted` stays false — but supplying it
    // lets verify check the cryptography instead of giving up.
    let output = sandbox.run_plugin(&[
        "verify",
        package.to_str().expect("package"),
        "--json",
        "--pubkey",
        pubkey.to_str().expect("pubkey"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("verify json");
    assert_eq!(value["signature"]["state"], "valid");
    assert_eq!(value["signature"]["trusted"], false);

    // With the key supplied, a tampered manifest is a hard failure rather than
    // an unknown-key shrug.
    let marker = b"test plugin";
    let offset = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("manifest desc");
    bytes[offset] = b'u';
    let tampered = sandbox.write_package("tampered-1.0.0.spk", &bytes);
    let output = sandbox.run_plugin(&[
        "verify",
        tampered.to_str().expect("package"),
        "--json",
        "--pubkey",
        pubkey.to_str().expect("pubkey"),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("verify failure json");
    assert!(
        value["error"]
            .as_str()
            .expect("error string")
            .contains("signature is invalid"),
        "{value}"
    );
}

#[test]
fn verify_reports_a_foreign_platform_as_a_host_fact() {
    let sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[20u8; 32]);
    sandbox.write_key(&key);

    let (name, data) = script();
    let path = format!("payload/bin/{}", name);
    let files = [(path.as_str(), data.as_slice(), 0o755u32)];
    let manifest = json!({
        "schema": 1,
        "name": "foreign",
        "version": "1.0.0",
        "desc": "other platform",
        "platform": other_target(),
        "protocol": 1,
        "exec": {"path": path.clone(), "args": [], "ready": "notify"},
        "files": [{
            "path": path,
            "size": data.len(),
            "sha256": hex_sha256(&data),
            "mode": 0o755
        }]
    });
    let bytes = package_with_manifest(&key, &manifest, &files);
    let package = sandbox.write_package("foreign-1.0.0.spk", &bytes);

    let output = sandbox.run_plugin(&["verify", package.to_str().expect("package"), "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("verify json");
    assert_eq!(value["ok"], true);
    assert_eq!(value["platform"], other_target());
    assert_eq!(value["signature"]["trusted"], true);
    assert_eq!(value["host"]["matchesHost"], false);
}

#[test]
fn bundle_refuses_missing_keys_ambiguous_execs_and_symlinks() {
    let sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[16u8; 32]);
    let key_path = sandbox.root.join("bundle.key");
    fs::write(&key_path, pem_from_private(&key)).expect("key");
    let source = sandbox.root.join("demo");
    let (name, data) = script();
    let exec = source.join("payload/bin").join(name);
    fs::create_dir_all(exec.parent().expect("exec parent")).expect("source");
    fs::write(&exec, &data).expect("exec");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let out = sandbox.root.join("out.spk");

    let no_key = sandbox.run_plugin(&[
        "bundle",
        source.to_str().expect("source"),
        "-o",
        out.to_str().expect("out"),
        "--id",
        "demo",
    ]);
    assert_eq!(no_key.status.code(), Some(1), "{}", stderr(&no_key));
    assert!(stderr(&no_key).contains("--key"), "{}", stderr(&no_key));
    assert!(!out.exists());

    let second = if cfg!(windows) {
        source.join("payload/bin/other.cmd")
    } else {
        source.join("payload/bin/other")
    };
    fs::write(&second, b"other").expect("second exec");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&second, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let ambiguous = sandbox.run_plugin(&[
        "bundle",
        source.to_str().expect("source"),
        "--key",
        key_path.to_str().expect("key"),
        "-o",
        out.to_str().expect("out"),
        "--id",
        "demo",
    ]);
    assert_eq!(ambiguous.status.code(), Some(1), "{}", stderr(&ambiguous));
    assert!(
        stderr(&ambiguous).contains("multiple executable candidates"),
        "{}",
        stderr(&ambiguous)
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = source.join("payload/bin/alias");
        let _ = fs::remove_file(&link);
        symlink(&exec, &link).expect("symlink");
        let output = sandbox.run_plugin(&[
            "bundle",
            source.to_str().expect("source"),
            "--key",
            key_path.to_str().expect("key"),
            "-o",
            out.to_str().expect("out"),
            "--id",
            "demo",
        ]);
        assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
        assert!(
            stderr(&output).contains("symbolic links are not allowed"),
            "{}",
            stderr(&output)
        );
    }
}

fn hook_body(kind: &str) -> Vec<u8> {
    #[cfg(windows)]
    {
        format!(
            "@echo off\r\necho %SVCD_PLUGIN_ID% %SVCD_PLUGIN_VERSION% %SVCD_ACTION%>\"%SVCD_DATA_DIR%\\{kind}.txt\"\r\n"
        )
        .into_bytes()
    }
    #[cfg(unix)]
    {
        format!(
            "#!/bin/sh\nprintf '%s %s %s\\n' \"$SVCD_PLUGIN_ID\" \"$SVCD_PLUGIN_VERSION\" \"$SVCD_ACTION\" > \"$SVCD_DATA_DIR/{kind}.txt\"\n"
        )
        .into_bytes()
    }
}

fn failing_hook_body() -> Vec<u8> {
    #[cfg(windows)]
    {
        b"@echo off\r\nexit /b 3\r\n".to_vec()
    }
    #[cfg(unix)]
    {
        b"#!/bin/sh\nexit 3\n".to_vec()
    }
}

fn package_with_hooks(
    key: &SigningKey,
    version: &str,
    post: Option<&[u8]>,
    pre: Option<&[u8]>,
) -> Vec<u8> {
    let (name, exec) = script();
    let exec_path = format!("payload/bin/{}", name);
    let config = b"{}".to_vec();
    let extension = if cfg!(windows) { "cmd" } else { "sh" };
    let mut files: Vec<(String, Vec<u8>, u32)> = vec![
        (exec_path.clone(), exec, 0o755),
        ("config/config".to_string(), config, 0o644),
    ];
    let mut hooks = serde_json::Map::new();
    if let Some(body) = post {
        let path = format!("hooks/post-install.{}", extension);
        files.push((path.clone(), body.to_vec(), 0o755));
        hooks.insert("postInstall".into(), json!(path));
    }
    if let Some(body) = pre {
        let path = format!("hooks/pre-remove.{}", extension);
        files.push((path.clone(), body.to_vec(), 0o755));
        hooks.insert("preRemove".into(), json!(path));
    }
    let entries: Vec<Value> = files
        .iter()
        .map(|(path, data, mode)| {
            json!({
                "path": path,
                "size": data.len(),
                "sha256": hex_sha256(data),
                "mode": mode
            })
        })
        .collect();
    let mut manifest = json!({
        "schema": 1,
        "name": "demo",
        "version": version,
        "desc": "test plugin",
        "platform": target(),
        "protocol": 1,
        "exec": {"path": exec_path, "args": [], "ready": "notify"},
        "config": "config/config",
        "files": entries
    });
    if !hooks.is_empty() {
        manifest["hooks"] = Value::Object(hooks);
    }
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
        tar.extend_from_slice(&header(&path, data.len(), mode));
        tar.extend_from_slice(&data);
        tar.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    tar.extend(std::iter::repeat_n(0, 1024));
    tar
}

fn read_marker(sandbox: &Sandbox, name: &str) -> String {
    fs::read_to_string(sandbox.data.join(name)).unwrap_or_else(|error| {
        panic!(
            "marker {} was not written: {} (daemon: {})",
            sandbox.data.join(name).display(),
            error,
            sandbox.daemon_log()
        )
    })
}

#[test]
fn install_and_uninstall_run_the_declared_hooks() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[21u8; 32]);
    sandbox.write_key(&key);
    let post = hook_body("post-install");
    let pre = hook_body("pre-remove");
    let package = sandbox.write_package(
        "demo-1.0.0.spk",
        &package_with_hooks(&key, "1.0.0", Some(&post), Some(&pre)),
    );
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let installed = sandbox.run_plugin(&["install", package.to_str().expect("utf8")]);
    assert_eq!(
        installed.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&installed)
    );
    assert_eq!(
        read_marker(&sandbox, "post-install.txt").trim(),
        "demo 1.0.0 post-install"
    );

    let removed = sandbox.run_plugin(&["uninstall", "demo"]);
    assert_eq!(
        removed.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&removed)
    );
    assert_eq!(
        read_marker(&sandbox, "pre-remove.txt").trim(),
        "demo 1.0.0 pre-remove"
    );
}

#[test]
fn a_failing_post_install_rolls_back_a_replace() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[22u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    let failing = failing_hook_body();
    let v2 = sandbox.write_package(
        "demo-1.1.0.spk",
        &package_with_hooks(&key, "1.1.0", Some(&failing), None),
    );
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );

    let failed = sandbox.run_plugin(&["install", v2.to_str().expect("utf8")]);
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains("post-install"),
        "{}",
        stderr(&failed)
    );

    let plugin = sandbox.data.join("plugins").join("demo");
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.0.0"
    );
    assert!(plugin.join("1.0.0").is_dir());
    assert!(
        !plugin.join("1.1.0").exists(),
        "failed new version was not removed"
    );
}

#[test]
fn a_failing_post_install_gets_no_hook_specific_rollback_from_keep_old() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[23u8; 32]);
    sandbox.write_key(&key);
    let v1 = sandbox.write_package("demo-1.0.0.spk", &package(&key, "1.0.0"));
    let failing = failing_hook_body();
    let v2 = sandbox.write_package(
        "demo-1.1.0.spk",
        &package_with_hooks(&key, "1.1.0", Some(&failing), None),
    );
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", v1.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );

    let failed = sandbox.run_plugin(&[
        "install",
        v2.to_str().expect("utf8"),
        "--strategy",
        "keep-old",
    ]);
    assert_eq!(failed.status.code(), Some(1), "{}", stderr(&failed));

    // keep-old keeps both versions and switches current; rollback policy is the
    // strategy's, not the post-install hook's.
    let plugin = sandbox.data.join("plugins").join("demo");
    assert_eq!(
        fs::read_to_string(plugin.join("current")).expect("current"),
        "1.1.0"
    );
    assert!(plugin.join("1.0.0").is_dir());
    assert!(plugin.join("1.1.0").is_dir());
}

#[test]
fn installs_and_supervises_a_script_service_with_lifecycle() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[25u8; 32]);
    sandbox.write_key(&key);
    let (exec, data) = clock_script();
    let manifest = clock_manifest(exec, &data);
    let bytes = package_with_manifest(&key, &manifest, &[(exec, &data, 0o755)]);
    let package = sandbox.write_package("clock-1.0.0.spk", &bytes);

    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let installed = sandbox.run_plugin(&["install", package.to_str().expect("package")]);
    assert_eq!(installed.status.code(), Some(0), "{}", stderr(&installed));

    let started = sandbox.run_plugin(&["start", "clock"]);
    assert_eq!(started.status.code(), Some(0), "{}", stderr(&started));

    let status = sandbox.run_plugin(&["status", "--json", "clock"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let value: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(value["summary"]["state"], "running", "{value}");
    assert!(
        value["details"]["probe"]
            .as_str()
            .unwrap_or("")
            .contains("clock ok"),
        "{value}"
    );

    let stopped = sandbox.run_plugin(&["stop", "clock"]);
    assert_eq!(stopped.status.code(), Some(0), "{}", stderr(&stopped));
    assert!(
        sandbox.data.join("clock.stopped").is_file(),
        "stop action did not run: {}",
        sandbox.daemon_log()
    );
}

#[test]
fn install_rejects_a_missing_interpreter() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[26u8; 32]);
    sandbox.write_key(&key);
    let (exec, data) = clock_script();
    let mut manifest = clock_manifest(exec, &data);
    manifest["interpreter"] = json!({"name": "svcd-no-such-interpreter"});
    let bytes = package_with_manifest(&key, &manifest, &[(exec, &data, 0o755)]);
    let package = sandbox.write_package("clock-1.0.0.spk", &bytes);

    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let installed = sandbox.run_plugin(&["install", package.to_str().expect("package")]);

    assert_ne!(installed.status.code(), Some(0), "{}", stderr(&installed));
    assert!(
        stderr(&installed).contains("not found"),
        "{}",
        stderr(&installed)
    );
    assert!(!sandbox.data.join("plugins/clock/current").exists());
}

#[test]
fn a_missing_interpreter_on_upgrade_leaves_the_old_version_running() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[27u8; 32]);
    sandbox.write_key(&key);
    let (exec, data) = clock_script();
    let mut manifest = clock_manifest(exec, &data);
    manifest["version"] = json!("1.0.0");
    let first = sandbox.write_package(
        "clock-1.0.0.spk",
        &package_with_manifest(&key, &manifest, &[(exec, &data, 0o755)]),
    );

    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", first.to_str().expect("package")])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        sandbox.run_plugin(&["start", "clock"]).status.code(),
        Some(0)
    );

    let mut broken = clock_manifest(exec, &data);
    broken["version"] = json!("1.1.0");
    broken["interpreter"] = json!({"name": "svcd-no-such-interpreter"});
    let second = sandbox.write_package(
        "clock-1.1.0.spk",
        &package_with_manifest(&key, &broken, &[(exec, &data, 0o755)]),
    );

    let failed = sandbox.run_plugin(&["install", second.to_str().expect("package")]);
    assert_ne!(failed.status.code(), Some(0), "{}", stderr(&failed));
    assert!(stderr(&failed).contains("not found"), "{}", stderr(&failed));

    let status = sandbox.run_plugin(&["status", "--json", "clock"]);
    assert_eq!(status.status.code(), Some(0), "{}", stderr(&status));
    let value: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(value["summary"]["state"], "running", "{value}");
    assert_eq!(value["summary"]["version"], "1.0.0", "{value}");
}

#[test]
fn a_failing_pre_remove_hook_does_not_block_uninstall() {
    let mut sandbox = Sandbox::new();
    let key = SigningKey::from_bytes(&[24u8; 32]);
    sandbox.write_key(&key);
    let failing = failing_hook_body();
    let package = sandbox.write_package(
        "demo-1.0.0.spk",
        &package_with_hooks(&key, "1.0.0", None, Some(&failing)),
    );
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert_eq!(
        sandbox
            .run_plugin(&["install", package.to_str().expect("utf8")])
            .status
            .code(),
        Some(0)
    );

    let removed = sandbox.run_plugin(&["uninstall", "demo"]);

    assert_eq!(removed.status.code(), Some(0), "{}", stderr(&removed));
    assert!(!sandbox
        .data
        .join("plugins")
        .join("demo")
        .join("current")
        .exists());
}
