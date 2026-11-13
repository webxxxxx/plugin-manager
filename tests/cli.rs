use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

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
        let root = std::env::temp_dir().join(format!(
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
        Command::new(&self.binary)
            .args(args)
            .env("SVCD_CONFIG_DIR", self.config_dir())
            .env("SVCD_LOG_DIR", self.logs_dir())
            .env("SVCD_TEST_UNPRIVILEGED", "1")
            .output()
            .expect("run svc")
    }
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

fn line_for<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines()
        .find(|line| line.split_whitespace().next() == Some(id))
        .unwrap_or_else(|| panic!("no line for {} in:\n{}", id, text))
}

fn has_row(text: &str, id: &str) -> bool {
    text.lines()
        .any(|line| line.split_whitespace().next() == Some(id))
}

#[test]
fn list_hides_the_builtin_and_shows_a_core_definition() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("demo", definition("demo"));

    let output = sandbox.run(&["list"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.starts_with("ID"), "table:\n{}", text);
    assert!(text.contains("KIND"));
    assert!(text.contains("STATE"));
    assert!(text.contains("AUTO"));
    assert!(text.contains("PID"));
    assert!(text.contains("VERSION"));

    let demo = line_for(&text, "demo");
    assert!(demo.contains("core"), "demo row: {}", demo);
    assert!(demo.contains("stopped"), "demo row: {}", demo);
    assert!(demo.contains("[auto]"), "demo row: {}", demo);
    assert!(demo.contains("1.0.0"), "demo row: {}", demo);

    assert!(!has_row(&text, "svc"), "table:\n{}", text);
}

#[test]
fn status_text_shows_summary_and_details() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("demo", definition("demo"));

    let output = sandbox.run(&["status", "demo"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("summary:"), "status:\n{}", text);
    assert!(text.contains("details:"), "status:\n{}", text);
    assert!(text.contains("id:"));
    assert!(text.contains("demo"));
    assert!(text.contains("kind:"));
    assert!(text.contains("core"));
    assert!(text.contains("state:"));
    assert!(text.contains("stopped"));
    assert!(text.contains("dependencies:"));
    assert!(text.contains("failures:"));
    assert!(text.contains("restart:"));
}

#[test]
fn status_json_is_exactly_summary_and_details() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("demo", definition("demo"));

    let output = sandbox.run(&["status", "demo", "--json"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let value: Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    let object = value.as_object().expect("object");
    assert_eq!(object.len(), 2);
    assert!(object.contains_key("summary"));
    assert!(object.contains_key("details"));

    let summary = &value["summary"];
    assert_eq!(summary["id"], "demo");
    assert_eq!(summary["kind"], "core");
    assert_eq!(summary["state"], "stopped");
    assert_eq!(summary["enabled"], true);
    assert!(summary["pid"].is_null());
    assert!(summary["since"].is_null());
    assert_eq!(summary["version"], "1.0.0");

    let details = &value["details"];
    assert_eq!(details["dependencies"], json!([]));
    assert!(details["last_exit"].is_null());
    assert!(details["next_at"].is_null());
    assert!(details["ready_since"].is_null());
    assert_eq!(details["failures"], 0);
    assert!(details["config"].is_null());
    assert!(details["restart"].is_null());
    assert_eq!(details["unkillable"], false);
}

#[test]
fn status_accepts_json_before_the_id() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("demo", definition("demo"));

    let output = sandbox.run(&["status", "--json", "demo"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let value: Value = serde_json::from_str(&stdout(&output)).expect("valid json");
    assert_eq!(value["summary"]["id"], "demo");
}

#[test]
fn an_unknown_service_exits_one() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["status", "missing"]);

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("unknown service"));
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

    for verb in ["start", "stop", "enable", "disable"] {
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

#[test]
fn an_spk_only_field_rejects_the_definition_with_a_reason() {
    let sandbox = Sandbox::new();
    let mut bad = definition("bad");
    bad["platform"] = json!("x86_64-unknown-linux-musl");
    sandbox.write_definition("bad", bad);

    let output = sandbox.run(&["list"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        !stdout(&output)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("bad")),
        "rejected definition leaked into the table:\n{}",
        stdout(&output)
    );
    let errors = stderr(&output);
    assert!(errors.contains("bad.json"), "stderr:\n{}", errors);
    assert!(errors.contains("platform"), "stderr:\n{}", errors);
    assert!(errors.contains(".spk-only"), "stderr:\n{}", errors);

    let status = sandbox.run(&["status", "bad"]);
    assert_eq!(status.status.code(), Some(1));
    assert!(stderr(&status).contains("platform"));
}

#[test]
fn the_reserved_svc_id_is_rejected_and_the_builtin_stays_hidden_from_list() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("svc", definition("svc"));

    let output = sandbox.run(&["list"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let errors = stderr(&output);
    assert!(errors.contains("reserved"), "stderr:\n{}", errors);
    let table = stdout(&output);
    assert!(!has_row(&table, "svc"), "table:\n{}", table);

    let status = sandbox.run(&["status", "svc"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(stdout(&status).contains("running"));
}

#[test]
fn a_duplicate_id_is_rejected_and_the_first_definition_wins() {
    let sandbox = Sandbox::new();
    sandbox.write_definition("a", definition("demo"));
    sandbox.write_definition("b", definition("demo"));

    let output = sandbox.run(&["list"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let demo_lines = stdout(&output)
        .lines()
        .filter(|line| line.split_whitespace().next() == Some("demo"))
        .count();
    assert_eq!(demo_lines, 1, "table:\n{}", stdout(&output));
    let errors = stderr(&output);
    assert!(
        errors.contains("duplicate service id"),
        "stderr:\n{}",
        errors
    );

    let status = sandbox.run(&["status", "demo"]);
    assert_eq!(status.status.code(), Some(0));
    assert!(stdout(&status).contains("id:"));
}

#[test]
fn a_relative_exec_path_is_rejected_with_a_readable_reason() {
    let sandbox = Sandbox::new();
    let mut bad = definition("bad");
    bad["exec"]["path"] = json!("payload/bad");
    sandbox.write_definition("bad", bad);

    let output = sandbox.run(&["list"]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let errors = stderr(&output);
    assert!(errors.contains("bad.json"), "stderr:\n{}", errors);
    assert!(errors.contains("absolute"), "stderr:\n{}", errors);
}

#[test]
fn trust_add_list_and_remove_manage_the_trust_store() {
    let sandbox = Sandbox::new();
    let public_key = sandbox.root.join("sample-2026.pub");
    fs::write(&public_key, include_str!("../samples/keys/sample-2026.pub"))
        .expect("write public key");

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
