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
            "svcd-daemon-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let config = root.join("config");
        let data = root.join("data");
        let logs = root.join("logs");
        let trust = config.join("trust.d");
        fs::create_dir_all(config.join("services.d")).expect("create sandbox");
        fs::create_dir_all(&data).expect("create data");
        fs::create_dir_all(&logs).expect("create logs");
        fs::create_dir_all(&trust).expect("create trust.d");

        let binary = env!("CARGO_BIN_EXE_svcd");
        let (svcd_name, svc_name, plugin_name) = if cfg!(windows) {
            ("svcd.exe", "svc.exe", "plugin.exe")
        } else {
            ("svcd", "svc", "plugin")
        };
        let svcd = root.join(svcd_name);
        let svc = root.join(svc_name);
        let plugin = root.join(plugin_name);
        for path in [&svcd, &svc, &plugin] {
            fs::copy(binary, path).expect("copy multicall");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                    .expect("chmod multicall");
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

    fn write_service(&self, id: &str, dependencies: &[&str], script: Script) {
        let script_path = match script {
            Script::Ready => self.write_ready_script(id),
            Script::CrashAfterReady => self.write_crash_after_ready_script(id),
            Script::Logging => self.write_logging_script(id),
        };
        let exec = exec_for(&script_path);
        let definition = json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "test service",
            "protocol": 1,
            "exec": exec,
            "dependencies": dependencies
        });
        fs::write(
            self.config.join("services.d").join(format!("{}.json", id)),
            serde_json::to_vec_pretty(&definition).expect("serialize definition"),
        )
        .expect("write definition");
    }

    fn write_ready_script(&self, id: &str) -> PathBuf {
        #[cfg(windows)]
        {
            let path = self.root.join(format!("{}.cmd", id));
            let script = "@echo off\r\nsetlocal\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\nendlocal\r\n";
            fs::write(&path, script).expect("write ready script");
            path
        }
        #[cfg(unix)]
        {
            let path = self.root.join(format!("{}.sh", id));
            let script = "#!/bin/sh\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n";
            fs::write(&path, script).expect("write ready script");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod ready script");
            path
        }
    }

    fn write_logging_script(&self, id: &str) -> PathBuf {
        #[cfg(windows)]
        {
            let path = self.root.join(format!("{}.cmd", id));
            let script = "@echo off\r\nsetlocal\r\necho hello-stdout\r\necho hello-stderr 1>&2\r\n:loop\r\nset \"line=\"\r\nset /p line=\r\nif errorlevel 1 goto end\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\ngoto loop\r\n:end\r\nendlocal\r\n";
            fs::write(&path, script).expect("write logging script");
            path
        }
        #[cfg(unix)]
        {
            let path = self.root.join(format!("{}.sh", id));
            let script = "#!/bin/sh\necho hello-stdout\necho hello-stderr >&2\nwhile IFS= read -r line; do\n  printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\ndone\n";
            fs::write(&path, script).expect("write logging script");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod logging script");
            path
        }
    }

    fn write_crash_after_ready_script(&self, id: &str) -> PathBuf {
        #[cfg(windows)]
        {
            let path = self.root.join(format!("{}.cmd", id));
            let script = "@echo off\r\nsetlocal\r\nset \"line=\"\r\nset /p line=\r\necho {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\r\nping -n 2 127.0.0.1 >nul\r\nexit /b 1\r\nendlocal\r\n";
            fs::write(&path, script).expect("write crash-after-ready script");
            path
        }
        #[cfg(unix)]
        {
            let path = self.root.join(format!("{}.sh", id));
            let script = "#!/bin/sh\nIFS= read -r line\nprintf '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\\n'\nsleep 1\nexit 1\n";
            fs::write(&path, script).expect("write crash-after-ready script");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod crash-after-ready script");
            path
        }
    }

    fn write_lifecycle_hook(&self, name: &str, _marker: &Path, keep_running: bool) -> PathBuf {
        #[cfg(windows)]
        {
            let path = self.root.join(format!("{}.cmd", name));
            let mut lines: Vec<String> = vec!["@echo off".into()];
            if keep_running {
                lines.push("setlocal".into());
            }
            lines.push(format!(r#"echo {}>>"%SVCD_TEST_MARKER%""#, name));
            if keep_running {
                lines.push(":loop".into());
                lines.push(r#"set "line=""#.into());
                lines.push("set /p line=".into());
                lines.push("if errorlevel 1 goto end".into());
                lines.push(r#"echo {"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.into());
                lines.push("goto loop".into());
                lines.push(":end".into());
                lines.push("endlocal".into());
            } else {
                lines.push("exit /b 0".into());
            }
            fs::write(&path, lines.join("\r\n") + "\r\n").expect("write lifecycle hook");
            path
        }
        #[cfg(unix)]
        {
            let path = self.root.join(format!("{}.sh", name));
            let mut lines: Vec<String> = vec!["#!/bin/sh".into()];
            lines.push(format!(r#"echo {} >> "$SVCD_TEST_MARKER""#, name));
            if keep_running {
                lines.push("while IFS= read -r line; do".into());
                lines.push(r#"  printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n'"#.into());
                lines.push("done".into());
            } else {
                lines.push("exit 0".into());
            }
            fs::write(&path, lines.join("\n") + "\n").expect("write lifecycle hook");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod lifecycle hook");
            path
        }
    }

    fn write_lifecycle_service(
        &self,
        id: &str,
        exec: &Path,
        start: &Path,
        stop: &Path,
        status: &Path,
        marker: &Path,
    ) {
        let hook = |path: &Path| {
            json!({
                "path": path.to_string_lossy(),
                "args": [],
                "env": { "SVCD_TEST_MARKER": marker.to_string_lossy() }
            })
        };
        let mut stop_hook = hook(stop);
        stop_hook["timeout"] = json!(5);
        let definition = json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "lifecycle test",
            "protocol": 1,
            "exec": { "path": exec.to_string_lossy(), "args": [], "ready": "notify" },
            "lifecycle": {
                "start": hook(start),
                "stop": stop_hook,
                "status": hook(status)
            }
        });
        fs::write(
            self.config.join("services.d").join(format!("{}.json", id)),
            serde_json::to_vec_pretty(&definition).expect("serialize definition"),
        )
        .expect("write lifecycle definition");
    }

    fn write_script_service(&self, id: &str) -> PathBuf {
        let directory = self.config.join("services.d");
        fs::create_dir_all(&directory).expect("services.d");
        #[cfg(windows)]
        {
            let path = directory.join(format!("{}.cmd", id));
            let lines: Vec<&str> = vec![
                "@echo off",
                "setlocal",
                r#"set "action=%~1""#,
                r#"if "%action%"=="" set "action=%SVCD_ACTION%""#,
                r#"if /I "%action%"=="start" goto start"#,
                r#"if /I "%action%"=="stop" goto stop"#,
                r#"if /I "%action%"=="status" goto status"#,
                "exit /b 64",
                ":start",
                r#"echo start>>"%~dp0marker.txt""#,
                ":loop",
                "ping -n 2 127.0.0.1 >nul",
                "goto loop",
                ":stop",
                r#"echo stop>>"%~dp0marker.txt""#,
                "exit /b 0",
                ":status",
                r#"echo status>>"%~dp0marker.txt""#,
                "exit /b 0",
            ];
            fs::write(&path, lines.join("\r\n") + "\r\n").expect("write script service");
            path
        }
        #[cfg(unix)]
        {
            let path = directory.join(format!("{}.sh", id));
            let lines: Vec<&str> = vec![
                "#!/bin/sh",
                "start() {",
                r#"  dir=$(dirname "$0")"#,
                r#"  echo start >> "$dir/marker.txt""#,
                "  while :; do sleep 1; done",
                "}",
                "stop() {",
                r#"  dir=$(dirname "$0")"#,
                r#"  echo stop >> "$dir/marker.txt""#,
                "}",
                "status() {",
                r#"  dir=$(dirname "$0")"#,
                r#"  echo status >> "$dir/marker.txt""#,
                "}",
                r#"action="${1:-$SVCD_ACTION}""#,
                r#"case "$action" in"#,
                r#"  start|stop|status) "$action" ;;"#,
                "  *) exit 64 ;;",
                "esac",
            ];
            fs::write(&path, lines.join("\n") + "\n").expect("write script service");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod script service");
            path
        }
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TRUST_DIR", &self.trust)
            .env("SVCD_TEST_UNPRIVILEGED", "1");
    }

    fn start_daemon(&mut self) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("create daemon log");
        let mut command = Command::new(&self.svcd);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        self.env(&mut command);
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    fn run_svc(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.svc);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svc")
    }

    fn run_svcd(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.svcd);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svcd")
    }

    fn run_plugin(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.plugin);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run plugin")
    }

    /// Kill the daemon without a shutdown RPC, as an OS crash or kill -9 would.
    fn force_kill_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn start_when_ready(&self, id: &str) -> Output {
        self.run_when_ready(&["start", id])
    }

    fn run_when_ready(&self, args: &[&str]) -> Output {
        for _ in 0..200 {
            let output = self.run_svc(args);
            if output.status.code() != Some(2) {
                return output;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("daemon never became reachable; log:\n{}", self.daemon_log());
    }

    fn status(&self, id: &str) -> Value {
        let output = self.run_svc(&["status", id, "--json"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "status {} failed: {}",
            id,
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("parse status")
    }

    fn stop(&self, id: &str) -> Output {
        self.run_svc(&["stop", id])
    }

    fn shutdown(&mut self) {
        let Some(mut child) = self.daemon.take() else {
            return;
        };
        let _ = self.run_svcd(&["shutdown"]);
        for _ in 0..200 {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    fn daemon_log(&self) -> String {
        fs::read_to_string(&self.daemon_stderr).unwrap_or_default()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.shutdown();
        let log = self.daemon_log();
        if !log.is_empty() {
            eprintln!("daemon log:\n{}", log);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

enum Script {
    Ready,
    CrashAfterReady,
    Logging,
}

fn exec_for(script: &Path) -> Value {
    #[cfg(windows)]
    {
        json!({
            "path": r"C:\Windows\System32\cmd.exe",
            "args": ["/c", script.to_string_lossy()],
            "ready": "notify"
        })
    }
    #[cfg(unix)]
    {
        json!({
            "path": "/bin/sh",
            "args": [script.to_string_lossy()],
            "ready": "notify"
        })
    }
}

#[test]
fn daemon_starts_a_ready_service_and_serves_live_status() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("demo", &[], Script::Ready);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );

    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["state"], "running");
    assert!(status["summary"]["pid"].as_u64().unwrap_or(0) > 0);

    let list = sandbox.run_svc(&["list"]);
    assert_eq!(list.status.code(), Some(0));
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(listed.contains("demo"), "list:\n{}", listed);
    assert!(listed.contains("running"), "list:\n{}", listed);

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["state"], "stopped");
    assert!(status["summary"]["pid"].is_null());
}

#[test]
fn service_stdout_and_stderr_are_logged() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("demo", &[], Script::Logging);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let output = sandbox.run_svc(&["logs", "demo", "--lines", "20"]);
        let text = String::from_utf8_lossy(&output.stdout);
        if text.contains("hello-stdout") && text.contains("hello-stderr") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "service logs did not appear: {}",
            text
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    let svcd = sandbox.run_svc(&["logs", "svcd"]);
    assert_eq!(svcd.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&svcd.stdout).contains("svcd starting"),
        "svcd log: {}",
        String::from_utf8_lossy(&svcd.stdout)
    );

    let missing = sandbox.run_svc(&["logs", "missing"]);
    assert_eq!(missing.status.code(), Some(1));
}

#[test]
fn start_pulls_dependencies_before_the_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("base", &[], Script::Ready);
    sandbox.write_service("app", &["base"], Script::Ready);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("app");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );
    assert_eq!(sandbox.status("base")["summary"]["state"], "running");
    assert_eq!(sandbox.status("app")["summary"]["state"], "running");

    assert_eq!(sandbox.stop("app").status.code(), Some(0));
    assert_eq!(sandbox.stop("base").status.code(), Some(0));
    assert_eq!(sandbox.status("app")["summary"]["state"], "stopped");
    assert_eq!(sandbox.status("base")["summary"]["state"], "stopped");

    let restarted = sandbox.run_svc(&["start", "app"]);
    assert_eq!(
        restarted.status.code(),
        Some(0),
        "restart failed: {}\n{}",
        String::from_utf8_lossy(&restarted.stderr),
        sandbox.daemon_log()
    );
    assert_eq!(sandbox.status("base")["summary"]["state"], "running");
    assert_eq!(sandbox.status("app")["summary"]["state"], "running");
}

#[test]
fn enable_disable_round_trip_and_boot_skip() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("demo", &[], Script::Ready);
    sandbox.start_daemon();

    assert_eq!(
        sandbox.start_when_ready("demo").status.code(),
        Some(0),
        "boot/start failed: {}",
        sandbox.daemon_log()
    );

    let disabled = sandbox.run_when_ready(&["disable", "demo"]);
    assert_eq!(
        disabled.status.code(),
        Some(0),
        "disable failed: {}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["enabled"], false);
    assert_eq!(status["summary"]["state"], "running");

    assert_eq!(sandbox.stop("demo").status.code(), Some(0));
    let restarted = sandbox.run_svc(&["start", "demo"]);
    assert_eq!(
        restarted.status.code(),
        Some(0),
        "explicit start of disabled service failed: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["enabled"], false);
    assert_eq!(sandbox.stop("demo").status.code(), Some(0));

    sandbox.shutdown();
    sandbox.start_daemon();
    let _ = sandbox.run_when_ready(&["disable", "demo"]);
    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["enabled"], false);
    assert_eq!(
        status["summary"]["state"], "stopped",
        "disabled service should not boot: {}",
        status
    );

    let enabled = sandbox.run_when_ready(&["enable", "demo"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "enable failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["enabled"], true);
}

#[test]
fn a_missing_dependency_rejects_start_with_its_id() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("app", &["missing"], Script::Ready);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("app");
    assert_eq!(
        started.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let error = String::from_utf8_lossy(&started.stderr);
    assert!(error.contains("missing dependencies"), "stderr: {}", error);
    assert!(error.contains("missing"), "stderr: {}", error);
}

#[test]
fn a_crashing_service_enters_backoff() {
    let mut sandbox = Sandbox::new();
    sandbox.write_service("crash", &[], Script::CrashAfterReady);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("crash");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start crashed: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = sandbox.status("crash");
        let state = status["summary"]["state"].as_str().unwrap_or_default();
        if state == "backoff" || state == "failed" {
            let failures = status["details"]["failures"].as_u64().unwrap_or(0);
            assert!(failures >= 1, "status: {}", status);
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crash never entered backoff: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn core_lifecycle_hooks_run_for_start_stop_and_status() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("marker.txt");
    let exec = sandbox.write_lifecycle_hook("exec", &marker, true);
    let start = sandbox.write_lifecycle_hook("start", &marker, true);
    let stop = sandbox.write_lifecycle_hook("stop", &marker, false);
    let status = sandbox.write_lifecycle_hook("status", &marker, false);
    sandbox.write_lifecycle_service("demo", &exec, &start, &stop, &status, &marker);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let text = fs::read_to_string(&marker).expect("marker");
    assert!(text.contains("start"), "start hook did not run: {text}");
    assert!(
        !text.contains("exec"),
        "exec ran instead of the start hook: {text}"
    );

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    let text = fs::read_to_string(&marker).expect("marker");
    assert!(text.contains("stop"), "stop hook did not run: {text}");

    let status_output = sandbox.run_svc(&["status", "demo", "--json"]);
    assert_eq!(
        status_output.status.code(),
        Some(0),
        "status failed: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );
    let value: Value = serde_json::from_slice(&status_output.stdout).expect("status json");
    assert_eq!(value["details"]["probe"], "ok");
    let text = fs::read_to_string(&marker).expect("marker");
    assert!(text.contains("status"), "status hook did not run: {text}");

    assert_eq!(sandbox.start_when_ready("demo").status.code(), Some(0));
    assert_eq!(sandbox.stop("demo").status.code(), Some(0));
    let text = fs::read_to_string(&marker).expect("marker");
    assert_eq!(
        text.lines().filter(|line| *line == "stop").count(),
        2,
        "stop hook must run again after a restart: {text}"
    );
}

/// A script service uses `exec.ready = Exec`, so `svc start` returns as soon
/// as the process is spawned: the script's marker appears asynchronously.
fn wait_for_marker(marker: &Path, needle: &str) {
    for _ in 0..200 {
        if fs::read_to_string(marker)
            .map(|text| text.contains(needle))
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "marker {} never contained {needle:?}: {:?}",
        marker.display(),
        fs::read_to_string(marker)
    );
}

#[test]
fn a_drop_in_script_is_discovered_and_supervised() {
    let mut sandbox = Sandbox::new();
    let script = sandbox.write_script_service("demo");
    assert!(script.is_file());
    sandbox.start_daemon();

    let list = sandbox.run_svc(&["list"]);
    assert!(
        String::from_utf8_lossy(&list.stdout)
            .lines()
            .any(|line| line.split_whitespace().next() == Some("demo")),
        "script service missing from list: {}",
        String::from_utf8_lossy(&list.stdout)
    );

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let marker = sandbox.config.join("services.d").join("marker.txt");
    wait_for_marker(&marker, "start");

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_for_marker(&marker, "stop");

    let status = sandbox.run_svc(&["status", "demo", "--json"]);
    assert_eq!(
        status.status.code(),
        Some(0),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let value: Value = serde_json::from_slice(&status.stdout).expect("status json");
    assert_eq!(value["details"]["probe"], "ok");
    wait_for_marker(&marker, "status");
}

// ---------------------------------------------------------------------------
// Lifecycle matrix fixtures: all three service sources, crash control and PIDs.
// ---------------------------------------------------------------------------

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

fn pid_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {}", pid), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{}\"", pid))
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

fn kill_pid(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
    }
}

fn wait_until(label: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Build a signed .spk. Kept local so the lifecycle tests own their plugin fixture.
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

impl Sandbox {
    /// A notify-mode agent: answers every stdin frame with its request id and
    /// exits on stdin EOF (the polite-stop path). Used for core JSON and plugin.
    fn write_agent(&self) -> PathBuf {
        #[cfg(windows)]
        {
            let path = self.root.join("agent.ps1");
            let script = r#"$ErrorActionPreference = 'Stop'
while ($null -ne ($line = [Console]::In.ReadLine())) {
  if ($line -notmatch '"id"\s*:\s*(\d+)') { continue }
  $requestId = [int]$Matches[1]
  [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $requestId + ',"result":{"ok":true}}')
  [Console]::Out.Flush()
}
"#;
            fs::write(&path, script).expect("write agent");
            path
        }
        #[cfg(unix)]
        {
            let path = self.root.join("agent.sh");
            let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  if [ -z "$id" ]; then continue; fi
  printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
done
"#;
            fs::write(&path, script).expect("write agent");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod agent");
            path
        }
    }

    /// A core service declared as services.d/<id>.json (ready = notify).
    fn write_core_json(&self, id: &str) {
        let agent = self.write_agent();
        #[cfg(windows)]
        let exec = json!({
            "path": r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", agent.to_string_lossy()],
            "ready": "notify"
        });
        #[cfg(unix)]
        let exec = json!({
            "path": "/bin/sh",
            "args": [agent.to_string_lossy()],
            "ready": "notify"
        });
        let definition = json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "lifecycle core",
            "protocol": 1,
            "exec": exec
        });
        fs::write(
            self.config.join("services.d").join(format!("{}.json", id)),
            serde_json::to_vec_pretty(&definition).expect("definition"),
        )
        .expect("write definition");
    }

    /// A drop-in script service that ignores stdin (ready = Exec) and exits when
    /// its stop action writes a sidecar file, so stops are fast on Windows too.
    fn write_stop_script(&self, id: &str) {
        #[cfg(windows)]
        {
            let path = self.config.join("services.d").join(format!("{}.ps1", id));
            let script = r#"param([string]$Action = $env:SVCD_ACTION)
$name = [IO.Path]::GetFileNameWithoutExtension($PSCommandPath)
$stopFile = Join-Path $PSScriptRoot ($name + '.stop')
switch ($Action) {
  'start' {
    Remove-Item $stopFile -Force -ErrorAction SilentlyContinue
    while (-not (Test-Path $stopFile)) { Start-Sleep -Milliseconds 100 }
    Remove-Item $stopFile -Force -ErrorAction SilentlyContinue
  }
  'stop' { Set-Content -Path $stopFile -Value 'stop' }
  'status' { exit 0 }
  default { exit 64 }
}
"#;
            fs::write(&path, script).expect("write script service");
        }
        #[cfg(unix)]
        {
            let path = self.config.join("services.d").join(format!("{}.sh", id));
            let script = r#"#!/bin/sh
dir=$(dirname "$0")
name=$(basename "$0" .sh)
stopfile="$dir/$name.stop"
action="$1"
if [ -z "$action" ]; then action="$SVCD_ACTION"; fi
case "$action" in
  start)
    rm -f "$stopfile"
    while [ ! -f "$stopfile" ]; do sleep 0.1; done
    rm -f "$stopfile"
    ;;
  stop) : > "$stopfile" ;;
  status) exit 0 ;;
  *) exit 64 ;;
esac
"#;
            fs::write(&path, script).expect("write script service");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
        }
    }

    /// Write the trust root and a signed plugin .spk; returns the package path.
    fn write_plugin_package(&self, id: &str) -> PathBuf {
        let key = SigningKey::from_bytes(&[0x5au8; 32]);
        fs::write(
            self.trust.join("test.pub"),
            pem_from_public(&key.verifying_key().to_bytes()),
        )
        .expect("trust key");
        let agent = self.write_agent();
        let exec_path = if cfg!(windows) {
            "payload/bin/agent.ps1".to_string()
        } else {
            "payload/bin/agent.sh".to_string()
        };
        let agent_bytes = fs::read(&agent).expect("read agent");
        let files = vec![(exec_path.clone(), agent_bytes, 0o755)];
        let manifest = json!({
            "schema": 1,
            "name": id,
            "version": "1.0.0",
            "desc": "lifecycle plugin",
            "platform": target(),
            "protocol": 1,
            "exec": {"path": exec_path, "args": [], "ready": "notify"},
            "capabilities": [
                {"name": "echo", "desc": "echo", "inputSchema": {"type": "object"}}
            ]
        });
        let package = signed_package(&key, manifest, &files);
        let path = self.root.join(format!("{}.spk", id));
        fs::write(&path, package).expect("write spk");
        path
    }

    /// Install a package through the daemon (the plugin multicall entry).
    fn install_plugin(&self, id: &str, package: &Path) {
        let output =
            self.run_when_ready_plugin(&["install", package.to_str().expect("package path")]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "install {}: {}",
            id,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_when_ready_plugin(&self, args: &[&str]) -> Output {
        for _ in 0..200 {
            let output = self.run_plugin(args);
            if output.status.code() != Some(2) {
                return output;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("daemon never became reachable; log:\n{}", self.daemon_log());
    }

    fn wait_for_state(&self, id: &str, expected: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status(id);
            if status["summary"]["state"].as_str() == Some(expected) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{} never reached {}: {}",
                id,
                expected,
                status
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn start_service(&self, id: &str) -> Value {
        let output = self.start_when_ready(id);
        assert_eq!(
            output.status.code(),
            Some(0),
            "start {}: {}",
            id,
            String::from_utf8_lossy(&output.stderr)
        );
        self.wait_for_state(id, "running", Duration::from_secs(10))
    }

    fn stop_service(&self, id: &str) -> Value {
        let output = self.stop(id);
        assert_eq!(
            output.status.code(),
            Some(0),
            "stop {}: {}",
            id,
            String::from_utf8_lossy(&output.stderr)
        );
        self.wait_for_state(id, "stopped", Duration::from_secs(15))
    }

    fn wait_for_restart(&self, id: &str, old_pid: u64, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status(id);
            let pid = status["summary"]["pid"].as_u64().unwrap_or(0);
            if status["summary"]["state"].as_str() == Some("running") && pid != old_pid && pid > 0 {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{} did not restart after pid {}: {}",
                id,
                old_pid,
                status
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn running_pid(&self, id: &str) -> u32 {
        let status = self.start_service(id);
        status["summary"]["pid"].as_u64().expect("pid") as u32
    }
}

// ---------------------------------------------------------------------------
// State matrix, crash restart and svcd crash coverage.
// ---------------------------------------------------------------------------

fn assert_state_matrix(sandbox: &Sandbox, id: &str, kind: &str, boot_started: bool) {
    let running = if boot_started {
        // Enabled services are started by the daemon at boot.
        sandbox.wait_for_state(id, "running", Duration::from_secs(15))
    } else {
        let initial = sandbox.status(id);
        assert_eq!(initial["summary"]["state"], "stopped", "{initial}");
        assert!(initial["summary"]["pid"].is_null(), "{initial}");
        sandbox.start_service(id)
    };
    let pid = running["summary"]["pid"].as_u64().expect("pid");
    assert!(pid > 0);
    assert_eq!(running["summary"]["kind"], kind, "{running}");
    assert_eq!(running["summary"]["enabled"], true, "{running}");
    assert_eq!(running["details"]["restart"], "on-failure", "{running}");
    assert_eq!(running["details"]["failures"], 0, "{running}");
    assert_eq!(running["summary"]["state"], "running");

    // Start is idempotent while running: same pid, still one instance.
    let again = sandbox.start_when_ready(id);
    assert_eq!(again.status.code(), Some(0));
    let same = sandbox.status(id);
    assert_eq!(same["summary"]["state"], "running");
    assert_eq!(same["summary"]["pid"].as_u64(), Some(pid));

    // Stop is synchronous, idempotent, and reaps the process.
    let stopped = sandbox.stop_service(id);
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert!(!pid_alive(pid as u32), "stopped pid {pid} is still alive");
    assert_eq!(sandbox.stop(id).status.code(), Some(0));
    assert_eq!(sandbox.status(id)["summary"]["state"], "stopped");

    // Explicit start works from Stopped.
    let restarted = sandbox.start_service(id);
    assert_eq!(restarted["summary"]["state"], "running");

    // disable flips the boot/crash-restart flag but keeps the process running.
    assert_eq!(
        sandbox.run_when_ready(&["disable", id]).status.code(),
        Some(0)
    );
    assert_eq!(sandbox.status(id)["summary"]["enabled"], false);
    assert_eq!(sandbox.status(id)["summary"]["state"], "running");
    sandbox.stop_service(id);

    // A disabled service can still be started explicitly.
    sandbox.start_service(id);
    assert_eq!(sandbox.status(id)["summary"]["enabled"], false);
    sandbox.stop_service(id);
    assert_eq!(
        sandbox.run_when_ready(&["enable", id]).status.code(),
        Some(0)
    );
    assert_eq!(sandbox.status(id)["summary"]["enabled"], true);
}

fn assert_crash_restart(sandbox: &Sandbox, id: &str) {
    let running = sandbox.start_service(id);
    let pid = running["summary"]["pid"].as_u64().expect("pid");

    kill_pid(pid as u32);
    let backoff = sandbox.wait_for_state(id, "backoff", Duration::from_secs(5));
    assert!(
        backoff["details"]["failures"].as_u64().unwrap_or(0) >= 1,
        "{backoff}"
    );
    assert!(!backoff["details"]["next_at"].is_null(), "{backoff}");

    let restarted = sandbox.wait_for_restart(id, pid, Duration::from_secs(15));
    assert_ne!(restarted["summary"]["pid"].as_u64(), Some(pid));
    assert_eq!(restarted["summary"]["state"], "running");
    sandbox.stop_service(id);
}

#[test]
fn state_matrix_for_core_json_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();
    assert_state_matrix(&sandbox, "core", "core", true);
}

#[test]
fn state_matrix_for_script_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon();
    assert_state_matrix(&sandbox, "script", "core", true);
}

#[test]
fn state_matrix_for_plugin_service() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_state_matrix(&sandbox, "plug", "plugin", false);
}

#[test]
fn crash_restart_for_core_json_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();
    assert_crash_restart(&sandbox, "core");
}

#[test]
fn crash_restart_for_script_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon();
    assert_crash_restart(&sandbox, "script");
}

#[test]
fn crash_restart_for_plugin_service() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_crash_restart(&sandbox, "plug");
}

#[test]
fn five_quick_crashes_enter_failed_and_explicit_start_resets() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("crashy");
    sandbox.start_daemon();

    let mut pid = sandbox.running_pid("crashy") as u64;
    for attempt in 1..=5 {
        kill_pid(pid as u32);
        if attempt < 5 {
            let restarted = sandbox.wait_for_restart("crashy", pid, Duration::from_secs(20));
            pid = restarted["summary"]["pid"].as_u64().expect("pid");
        }
    }

    let failed = sandbox.wait_for_state("crashy", "failed", Duration::from_secs(25));
    assert_eq!(failed["details"]["failures"], 5, "{failed}");
    assert!(failed["summary"]["pid"].is_null(), "{failed}");

    // Only an explicit start leaves Failed.
    let running = sandbox.start_service("crashy");
    assert_eq!(running["details"]["failures"], 0, "{running}");
    sandbox.stop_service("crashy");
}

#[test]
fn a_disabled_service_crashes_to_stopped_without_restart() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("off");
    sandbox.start_daemon();
    assert_eq!(
        sandbox.run_when_ready(&["disable", "off"]).status.code(),
        Some(0)
    );

    let pid = sandbox.running_pid("off") as u64;
    kill_pid(pid as u32);
    let backoff = sandbox.wait_for_state("off", "backoff", Duration::from_secs(5));
    assert!(
        backoff["details"]["failures"].as_u64().unwrap_or(0) >= 1,
        "{backoff}"
    );

    let stopped = sandbox.wait_for_state("off", "stopped", Duration::from_secs(10));
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert!(!pid_alive(pid as u32));
}

#[test]
fn graceful_svcd_restart_boots_enabled_services_and_skips_disabled() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.write_stop_script("script");
    sandbox.write_core_json("off");
    fs::write(sandbox.config.join("services.d").join("off.disabled"), b"")
        .expect("disabled marker");

    sandbox.start_daemon();
    let package = sandbox.write_plugin_package("plug");
    sandbox.install_plugin("plug", &package);

    let mut pids = Vec::new();
    for id in ["core", "script", "plug"] {
        pids.push(sandbox.running_pid(id) as u64);
    }

    sandbox.shutdown();
    for pid in &pids {
        wait_until(
            &format!("pid {} to exit", pid),
            Duration::from_secs(10),
            || !pid_alive(*pid as u32),
        );
    }

    sandbox.start_daemon();
    for id in ["core", "script", "plug"] {
        let status = sandbox.wait_for_state(id, "running", Duration::from_secs(15));
        assert!(
            status["summary"]["pid"].as_u64().unwrap_or(0) > 0,
            "{status}"
        );
    }
    let off = sandbox.status("off");
    assert_eq!(off["summary"]["state"], "stopped", "{off}");
    assert_eq!(off["summary"]["enabled"], false, "{off}");
}

#[test]
fn builtin_svc_stays_running_and_ignores_start_stop() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();

    let status = sandbox.status("svc");
    assert_eq!(status["summary"]["state"], "running", "{status}");
    assert_eq!(status["summary"]["kind"], "core", "{status}");
    assert_eq!(status["summary"]["enabled"], true, "{status}");

    assert_eq!(
        sandbox.run_when_ready(&["start", "svc"]).status.code(),
        Some(0)
    );
    assert_eq!(
        sandbox.run_when_ready(&["stop", "svc"]).status.code(),
        Some(0)
    );
    assert_eq!(sandbox.status("svc")["summary"]["state"], "running");
}

/// An abrupt svcd death must take its supervised processes with it: the OS
/// parent-death protection (Windows job object / Linux PDEATHSIG) reaps every
/// child, so the restarted daemon starts exactly one instance per service and
/// never meets a leftover from the previous run.
#[test]
fn abrupt_svcd_exit_reaps_children_and_restart_starts_one_instance() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.write_stop_script("script");
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);

    let core_pid = sandbox.running_pid("core") as u64;
    let script_pid = sandbox.running_pid("script") as u64;
    let plugin_pid = sandbox.running_pid("plug") as u64;

    sandbox.force_kill_daemon();

    wait_until(
        "all children to die with svcd",
        Duration::from_secs(10),
        || {
            !pid_alive(core_pid as u32)
                && !pid_alive(script_pid as u32)
                && !pid_alive(plugin_pid as u32)
        },
    );

    sandbox.start_daemon();
    for (id, old_pid) in [
        ("core", core_pid),
        ("script", script_pid),
        ("plug", plugin_pid),
    ] {
        let status = sandbox.wait_for_restart(id, old_pid, Duration::from_secs(15));
        assert!(
            status["summary"]["pid"].as_u64().unwrap_or(0) > 0,
            "{status}"
        );
        assert!(
            !pid_alive(old_pid as u32),
            "{}: the pre-crash instance is still alive",
            id
        );
    }

    sandbox.stop_service("core");
    sandbox.stop_service("script");
    sandbox.stop_service("plug");
}
