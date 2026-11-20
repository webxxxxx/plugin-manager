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

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
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

    /// Run an installed alias (`svc` / `plugin` / `mcp` / `svcd`) from this
    /// sandbox. The multicall binary dispatches on its file stem.
    fn run_as(&self, name: &str, args: &[&str]) -> Output {
        let suffix = if cfg!(windows) { ".exe" } else { "" };
        let binary = self.root.join(format!("{}{}", name, suffix));
        if binary == self.binary {
            return self.run(args);
        }
        // Windows reports a sharing violation while another sandbox's copy of
        // the freshly written binary is still being scanned or released.
        let mut last = None;
        for _ in 0..50 {
            match fs::copy(&self.binary, &binary) {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(error) => {
                    last = Some(error);
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        if let Some(error) = last {
            panic!("copy alias {name}: {error}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
                .expect("make alias executable");
        }
        let mut command = Command::new(&binary);
        command
            .args(args)
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_DATA_DIR", self.data_dir())
            .env("SVCD_LOG_DIR", self.logs_dir())
            .env("SVCD_TEST_UNPRIVILEGED", "1");
        command.output().expect("run alias")
    }

    /// The same command with extra environment, for the keys a test needs to
    /// pin (`SVCD_DATA_DIR`) without changing what `run` sets.
    fn run_env(&self, args: &[&str], envs: &[(&str, &std::path::Path)]) -> Output {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_LOG_DIR", self.logs_dir())
            .env("SVCD_TEST_UNPRIVILEGED", "1");
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("run svc with env")
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.binary);
        command
            .args(args)
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_DATA_DIR", self.data_dir())
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

/// The config is derived once at each entry. A broken file is a hard error
/// instead of a silent fallback to defaults; `--help`/usage stays available
/// because it never bootstraps.
#[test]
fn a_broken_config_is_a_hard_error_for_every_entry() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.config_dir()).expect("config dir");
    fs::write(
        sandbox.config_dir().join("svcd.conf"),
        "bind = not-an-address\n",
    )
    .expect("broken conf");

    for (name, args) in [("svc", &["list"][..]), ("plugin", &["list"][..])] {
        let output = sandbox.run_as(name, args);
        assert_eq!(output.status.code(), Some(1), "{name}: {}", stderr(&output));
        assert!(
            stderr(&output).contains("invalid config"),
            "{name}: {}",
            stderr(&output)
        );
    }

    // The daemon entry reports it before touching any service.
    let conf = sandbox.config_dir().join("svcd.conf");
    let mut daemon_command = Command::new(env!("CARGO_BIN_EXE_svcd"));
    daemon_command
        .arg("run")
        .arg("--foreground")
        .arg("--conf")
        .arg(&conf)
        .env("SVCD_CONFIG_DIR", sandbox.config_dir());
    let daemon = daemon_command.output().expect("run daemon entry");
    assert_eq!(daemon.status.code(), Some(1), "{}", stderr(&daemon));
    assert!(
        stderr(&daemon).contains("invalid config"),
        "{}",
        stderr(&daemon)
    );

    // MCP is an entry too: no stdin is read before the bootstrap fails. The
    // multicall binary dispatches on its file stem, so it must be `mcp`.
    let mcp = sandbox.run_as("mcp", &[]);
    assert_eq!(mcp.status.code(), Some(1), "{}", stderr(&mcp));
    assert!(stderr(&mcp).contains("invalid config"), "{}", stderr(&mcp));

    // Usage does not need a config.
    assert_eq!(sandbox.run(&["--help"]).status.code(), Some(64));

    // Stopping only pings the control endpoint: a broken config must not turn
    // a stop into "invalid config" (there would be no way to stop the daemon).
    let stop = sandbox.run_as("svcd", &["shutdown"]);
    assert_eq!(stop.status.code(), Some(2), "{}", stderr(&stop));
    assert!(
        !stderr(&stop).contains("invalid config"),
        "{}",
        stderr(&stop)
    );
}

/// `plugin bundle` works on a directory: it needs no runtime directories, so a
/// broken config must not block it (only the entries that need a directory
/// bootstrap).
#[test]
fn plugin_bundle_does_not_need_a_runtime_config() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.config_dir()).expect("config dir");
    fs::write(sandbox.config_dir().join("svcd.conf"), "bind = nope\n").expect("broken conf");

    let source = sandbox.root.join("source");
    fs::create_dir_all(source.join("payload/bin")).expect("payload");
    let exec = source.join("payload/bin/demo");
    fs::write(&exec, b"#!/bin/sh\nexit 0\n").expect("exec");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let out = sandbox.root.join("demo.spk");
    let output = sandbox.run_as(
        "plugin",
        &[
            "bundle",
            source.to_str().expect("utf8"),
            "--unsigned",
            "--id",
            "demo",
            "--exec",
            "payload/bin/demo",
            "-o",
            out.to_str().expect("utf8"),
        ],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(out.is_file(), "bundle output missing");
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

#[test]
fn config_tune_prints_a_document_that_derives_the_same_values() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_as("svc", &["config", "tune"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = stdout(&output);
    for needle in [
        "# machine:",
        "memory budget:",
        "% of the",
        "mem_budget_bytes =",
        "workload =",
        "max_connections =",
        "# disk (does not enter the memory budget):",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }

    // The printed text is a document: writing it back must derive the same
    // numbers, which is what "写入后运行时以 svcd.conf 为准" means.
    fs::create_dir_all(sandbox.config_dir()).expect("config dir");
    fs::write(sandbox.config_dir().join("svcd.conf"), &text).expect("write tuned conf");
    let again = sandbox.run_as("svc", &["config", "tune"]);
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let tuned = |output: &Output| {
        stdout(output)
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        tuned(&again),
        tuned(&output),
        "a tuned document must not drift"
    );
}

#[test]
fn config_tune_write_keeps_one_block_and_the_rest_of_the_document() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.config_dir()).expect("config dir");
    let conf = sandbox.config_dir().join("svcd.conf");
    fs::write(&conf, "bind = 127.0.0.1:8787\n").expect("write conf");

    let output = sandbox.run_as("svc", &["config", "tune", "--write"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&conf).expect("read conf");
    assert!(text.starts_with("bind = 127.0.0.1:8787\n"), "{text}");
    assert!(text.contains("# BEGIN svcd config tune"), "{text}");
    assert!(text.contains("max_connections ="), "{text}");

    // Re-running owns exactly one block, and the document still loads.
    let output = sandbox.run_as("svc", &["config", "tune", "--write"]);
    assert_eq!(output.status.code(), Some(0));
    let text = fs::read_to_string(&conf).expect("read conf");
    assert_eq!(
        text.matches("# BEGIN svcd config tune").count(),
        1,
        "{text}"
    );
    assert_eq!(text.matches("# END svcd config tune").count(), 1, "{text}");

    let output = sandbox.run_as("svc", &["config", "tune"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "the written document must load: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn status_resources_reports_the_last_start_and_falls_back_without_one() {
    let sandbox = Sandbox::new();
    let data = sandbox.root.join("data");
    fs::create_dir_all(data.join("run").join("resources")).expect("data dir");

    // No snapshot yet: the command still answers and says where the numbers
    // came from.
    let output = sandbox.run_env(
        &["status", "--resources", "--json"],
        &[("SVCD_DATA_DIR", &data)],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("resource report");
    assert!(report["budget_quantized"].as_u64().unwrap() >= 64 * 1024 * 1024);
    assert!(report["worst_case"].as_u64().unwrap() <= report["budget_quantized"].as_u64().unwrap());

    // A snapshot stands in for the last start.
    fs::write(
        data.join("run").join("resources").join("startup.json"),
        serde_json::to_vec_pretty(&report).expect("serialize"),
    )
    .expect("write snapshot");
    let output = sandbox.run_env(&["status", "--resources"], &[("SVCD_DATA_DIR", &data)]);
    assert_eq!(output.status.code(), Some(0));
    let text = stdout(&output);
    for needle in ["source:  last start", "memory:", "workload:", "worst case:"] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }

    let plain = sandbox.run_env(&["status", "--resources"], &[]);
    assert!(
        stdout(&plain).contains("this process"),
        "without a snapshot the source must say so:\n{}",
        stdout(&plain)
    );
}

#[test]
fn an_unrunnable_document_is_still_inspectable() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.config_dir()).expect("config dir");
    // A 64 MiB budget with an 8 MiB stack on ten threads cannot be planned.
    fs::write(
        sandbox.config_dir().join("svcd.conf"),
        "mem_budget_bytes = 67108864\nrequest_stack_bytes = 8388608\nblocking_threads = 8\n",
    )
    .expect("write conf");

    // The status and tuning views still answer: they are how the operator finds
    // out what is wrong.
    let output = sandbox.run(&["status", "--resources"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout(&output).contains("refused:"),
        "the refusal must be visible:\n{}",
        stdout(&output)
    );

    let output = sandbox.run(&["config", "tune"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout(&output).contains("WARNING: the daemon refuses to start"),
        "the tuning output must warn:\n{}",
        stdout(&output)
    );

    // The process entry that would run the daemon keeps refusing, with a
    // readable reason and a non-zero exit code. `user status` goes through the
    // strict bootstrap without needing a daemon.
    let output = sandbox.run_as("svcd", &["user", "status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("does not fit"),
        "{}",
        stderr(&output)
    );
}
