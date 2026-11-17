use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

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
    binary: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = sandbox_base().join(format!(
            "svcd-cli-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create sandbox");
        fs::create_dir_all(root.join("logs")).expect("create logs");

        let binary_name = if cfg!(windows) { "svc.exe" } else { "svc" };
        let binary = root.join(binary_name);
        fs::copy(env!("CARGO_BIN_EXE_svcd"), &binary).expect("copy multicall binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
                .expect("make svc executable");
        }

        Sandbox { root, binary }
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    fn write_definition(&self, name: &str, definition: Value) {
        let directory = self.config_dir().join("services.d");
        fs::create_dir_all(&directory).expect("create services.d");
        fs::write(
            directory.join(format!("{}.json", name)),
            serde_json::to_vec_pretty(&definition).expect("serialize definition"),
        )
        .expect("write definition");
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_LOG_DIR", self.logs_dir())
            .env("SVCD_TEST_UNPRIVILEGED", "1");
        // Parallel tests can briefly hit ETXTBSY while another sandbox copies
        // the multicall binary on a shared filesystem. Retry the exec only.
        for attempt in 0..50 {
            match command.output() {
                Ok(output) => return output,
                Err(error) if is_executable_busy(&error) && attempt < 49 => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("run svc: {error}"),
            }
        }
        unreachable!("the retry loop returns or panics")
    }
}

fn is_executable_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ExecutableFileBusy || error.raw_os_error() == Some(26)
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn absolute_exec() -> String {
    if cfg!(windows) {
        r"C:\Windows\System32\cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn definition(id: &str) -> Value {
    json!({
        "schema": 1,
        "id": id,
        "version": "1.0.0",
        "desc": "demo service",
        "protocol": 1,
        "exec": { "path": absolute_exec(), "args": [], "ready": "exec" },
        "dependencies": []
    })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn list_without_a_daemon_reports_unreachable() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("demo", definition("demo"));

    let output = sandbox.run(&["list"]);

    // The offline fallback is gone: a reachable daemon is required, and a dead
    // one must be reported instead of printing an empty table.
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("svcd unreachable"),
        "stderr:\n{}",
        stderr(&output)
    );
}

#[test]
fn usage_errors_exit_sixty_four() {
    let sandbox = Sandbox::new();

    for args in [
        vec![],
        vec!["status"],
        vec!["status", "one", "two"],
        vec!["list", "--json"],
        vec!["describe", "demo"],
        vec!["logs"],
        vec!["logs", "demo", "--lines", "nope"],
    ] {
        let output = sandbox.run(&args);
        assert_eq!(
            output.status.code(),
            Some(64),
            "args {:?} stderr: {}",
            args,
            stderr(&output)
        );
        assert!(stderr(&output).contains("usage:"));
    }
}

#[test]
fn daemon_commands_exit_two_until_the_channel_exists() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("demo", definition("demo"));

    for verb in ["start", "stop", "restart", "enable", "disable"] {
        let output = sandbox.run(&[verb, "demo"]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{} stderr: {}",
            verb,
            stderr(&output)
        );
        assert!(stderr(&output).contains("unreachable"));
    }
}

#[test]
fn logs_print_the_tail_without_a_daemon() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.logs_dir().join("demo.log"), "one\ntwo\nthree\n").expect("write log");

    let output = sandbox.run(&["logs", "demo", "--lines", "2"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "two\nthree\n");
}

#[test]
fn logs_read_svcd_log_and_missing_logs_exit_one() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.logs_dir().join("svcd.log"), "svcd starting\n").expect("write log");

    let output = sandbox.run(&["logs", "svcd"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stdout(&output), "svcd starting\n");

    let missing = sandbox.run(&["logs", "missing"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("no log"));
}

/// Test-only Ed25519 public key. `samples/keys/` is generated locally by
/// `tools/gen_sample_key.mjs` and never committed, so tests must not read it.
const SAMPLE_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\n\
MCowBQYDK2VwAyEAOLVjY8cVextWNtjO6wCg0g25/7mN+wRpMkd597CV3Nk=\n\
-----END PUBLIC KEY-----\n";

#[test]
fn compat_reports_and_provisions_the_device_id() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["compat"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("platform:"), "compat:\n{}", text);
    assert!(text.contains("device:        -"), "compat:\n{}", text);

    let json = sandbox.run(&["compat", "--json"]);
    assert_eq!(json.status.code(), Some(0));
    let value: Value = serde_json::from_str(&stdout(&json)).expect("compat json");
    assert_eq!(value["schema_max"], 3);
    assert!(value["device"].is_null(), "{value}");

    let init = sandbox.run(&["compat", "init", "--device", "sr6110"]);
    assert_eq!(init.status.code(), Some(0), "stderr: {}", stderr(&init));
    assert!(sandbox.config_dir().join("compat.conf").is_file());

    let text = stdout(&sandbox.run(&["compat"]));
    assert!(text.contains("sr6110"), "compat:\n{}", text);

    let bad = sandbox.run(&["compat", "init", "--device", "SR6110"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("device id"), "{}", stderr(&bad));
}

#[test]
fn trust_add_list_and_remove_manage_the_trust_store() {
    let sandbox = Sandbox::new();
    let public_key = sandbox.root.join("sample-2026.pub");
    fs::write(&public_key, SAMPLE_PUBLIC_KEY).expect("write public key");

    let added = sandbox.run(&["trust", "add", public_key.to_str().expect("utf8")]);
    assert_eq!(added.status.code(), Some(0), "stderr: {}", stderr(&added));
    assert!(stdout(&added).contains("sample-2026"));
    let stored = sandbox.config_dir().join("trust.d").join("sample-2026.pub");
    assert!(stored.is_file());

    let listed = sandbox.run(&["trust", "list"]);
    assert_eq!(listed.status.code(), Some(0), "stderr: {}", stderr(&listed));
    assert!(stdout(&listed).contains("sample-2026"));

    let removed = sandbox.run(&["trust", "remove", "sample-2026"]);
    assert_eq!(
        removed.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&removed)
    );
    assert!(!stored.exists());

    let missing = sandbox.run(&["trust", "remove", "sample-2026"]);
    assert_eq!(missing.status.code(), Some(1));

    let bad = sandbox.root.join("bad.pub");
    fs::write(&bad, "not a public key").expect("bad key");
    let rejected = sandbox.run(&["trust", "add", bad.to_str().expect("utf8")]);
    assert_eq!(
        rejected.status.code(),
        Some(1),
        "stdout: {}",
        stdout(&rejected)
    );
    assert!(!sandbox
        .config_dir()
        .join("trust.d")
        .join("bad.pub")
        .exists());
}
