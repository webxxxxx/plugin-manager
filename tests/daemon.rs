use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    root: PathBuf,
    config: PathBuf,
    logs: PathBuf,
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
            "svcd-daemon-{}-{}-{}",
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let config = root.join("config");
        let logs = root.join("logs");
        fs::create_dir_all(config.join("services.d")).expect("create sandbox");
        fs::create_dir_all(&logs).expect("create logs");

        let binary = env!("CARGO_BIN_EXE_svcd");
        let (svcd_name, svc_name) = if cfg!(windows) {
            ("svcd.exe", "svc.exe")
        } else {
            ("svcd", "svc")
        };
        let svcd = root.join(svcd_name);
        let svc = root.join(svc_name);
        fs::copy(binary, &svcd).expect("copy svcd");
        fs::copy(binary, &svc).expect("copy svc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&svcd, &svc] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                    .expect("chmod multicall");
            }
        }

        Sandbox {
            daemon_stderr: root.join("daemon.stderr"),
            root,
            config,
            logs,
            svcd,
            svc,
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

    fn start_daemon(&mut self) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("create daemon log");
        let child = Command::new(&self.svcd)
            .args(["run", "--foreground"])
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_LOG_DIR", &self.logs)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn daemon");
        self.daemon = Some(child);
    }

    fn run_svc(&self, args: &[&str]) -> Output {
        Command::new(&self.svc)
            .args(args)
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_LOG_DIR", &self.logs)
            .output()
            .expect("run svc")
    }

    fn run_svcd(&self, args: &[&str]) -> Output {
        Command::new(&self.svcd)
            .args(args)
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_LOG_DIR", &self.logs)
            .output()
            .expect("run svcd")
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
