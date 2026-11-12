use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
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
    plugin: PathBuf,
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
            plugin,
            daemon: None,
        }
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TRUST_DIR", &self.trust);
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

    fn run(&self, binary: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(binary);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svcd entry")
    }

    fn run_plugin(&self, args: &[&str]) -> Output {
        self.run(&self.plugin, args)
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

    fn shutdown(&mut self) {
        if self.daemon.is_some() {
            let _ = self.run(&self.svcd, &["shutdown"]);
            if let Some(mut child) = self.daemon.take() {
                let _ = child.wait();
            }
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
    let (name, data) = script();
    let path = format!("payload/bin/{}", name);
    let config = b"{}".to_vec();
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

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("json status")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
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
            .run_plugin(&["install", v2.to_str().expect("utf8")])
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
