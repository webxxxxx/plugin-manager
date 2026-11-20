use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

mod support;

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
        support::install_multicall(Path::new(binary), &[&svcd, &svc, &plugin]);

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
            stop_timeout: support::STOP_TIMEOUT,
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

    fn write_script_body(&self, id: &str, body: &str) -> PathBuf {
        #[cfg(windows)]
        let name = format!("{}.cmd", id);
        #[cfg(unix)]
        let name = format!("{}.sh", id);
        self.write_script_as(&name, body)
    }

    fn write_script_as(&self, name: &str, body: &str) -> PathBuf {
        let directory = self.config.join("services.d");
        fs::create_dir_all(&directory).expect("services.d");
        let path = directory.join(name);
        fs::write(&path, body).expect("write script body");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
        }
        path
    }

    fn env(&self, command: &mut Command) {
        command
            .env("SVCD_CONFIG_DIR", &self.config)
            .env("SVCD_DATA_DIR", &self.data)
            .env("SVCD_LOG_DIR", &self.logs)
            .env("SVCD_TEST_UNPRIVILEGED", "1")
            // Tests drive definitions explicitly; the periodic rescan is opt-in
            // per test via `start_daemon_with_env`.
            .env("SVCD_RESCAN_INTERVAL_MS", "0");
    }

    fn start_daemon(&mut self) {
        self.start_daemon_with_env(&[]);
    }

    /// Spawn the daemon and wait until its control channel answers. Tests that
    /// call `svc`/`status` immediately must use this: the CLI exits
    /// `EXIT_DAEMON_UNREACHABLE` until the daemon's RPC loop is live.
    fn start_daemon_ready(&mut self) {
        self.start_daemon();
        self.wait_for_daemon();
    }

    fn start_daemon_with_env(&mut self, extra: &[(&str, &str)]) {
        let stderr = fs::File::create(&self.daemon_stderr).expect("create daemon log");
        let mut command = Command::new(&self.svcd);
        command
            .args(["run", "--foreground"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        self.env(&mut command);
        for (key, value) in extra {
            command.env(key, value);
        }
        self.daemon = Some(command.spawn().expect("spawn daemon"));
    }

    fn run_svc(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.svc);
        command.args(args);
        self.env(&mut command);
        command.output().expect("run svc")
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

    /// Wait until the daemon answers over the control channel.
    ///
    /// `svc reload` is the readiness probe: it has no offline fallback and
    /// exits EXIT_DAEMON_UNREACHABLE until the daemon accepts. Answering it
    /// also proves the boot scan finished (the RPC is handled by the same
    /// loop).
    fn wait_for_daemon(&self) {
        let output = self.run_when_ready(&["reload"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "reload: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

    /// Stop the daemon with a bounded request and a bounded wait; see
    /// [`support::stop_daemon`].
    fn stop_daemon(&mut self) -> Result<(), String> {
        let config = self.config.clone();
        self.stop_daemon_at(&config)
    }

    /// Stop, but target `config` instead of the sandbox's own. The Unix
    /// socket derives from data/run and the Windows pipe name from config, so
    /// moving both makes the request genuinely unreachable on every host.
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

    fn daemon_log(&self) -> String {
        fs::read_to_string(&self.daemon_stderr).unwrap_or_default()
    }
}

/// The suite already bounded this wait; it now shares the helper with the
/// other sandboxes, so the request has a bound too.
#[test]
fn stop_wait_is_bounded_when_the_request_cannot_reach_the_daemon() {
    let mut sandbox = Sandbox::new();
    let timeout = Duration::from_secs(1);
    sandbox.stop_timeout = timeout;
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    let unreachable = sandbox.root.join("no-daemon");
    fs::create_dir_all(&unreachable).expect("unreachable config");

    support::assert_bounded_stop(|| sandbox.stop_daemon_at(&unreachable), timeout);
    assert!(sandbox.daemon.is_none(), "the handle must be consumed");
}

/// The channel authorizes by endpoint ACL now; a token left by the previous
/// version must not survive a start.
#[test]
fn the_legacy_control_token_is_removed_on_start() {
    let mut sandbox = Sandbox::new();
    fs::write(sandbox.config.join("svcd.token"), "legacy-token").expect("legacy token");
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    assert!(
        !sandbox.config.join("svcd.token").exists(),
        "the daemon must remove the token it no longer reads"
    );
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
    assert!(
        !listed
            .lines()
            .any(|line| line.split_whitespace().next() == Some("svc")),
        "list:\n{}",
        listed
    );

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
        "explicit start of disabled service failed: {}\n{}",
        String::from_utf8_lossy(&restarted.stderr),
        sandbox.daemon_log()
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
fn a_notify_one_shot_definition_never_becomes_a_startable_service() {
    // #197: accepting `remain_after_exit` on a notify-ready definition would
    // let `svc start` settle on Running with no pid while the stdio channel is
    // already gone. The definition is rejected at load, so no service exists.
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("odd");
    sandbox.set_definition_field("odd", "remain_after_exit", json!(true));

    sandbox.start_daemon();

    let started = sandbox.start_when_ready("odd");
    assert_eq!(
        started.status.code(),
        Some(1),
        "stderr: {}\n{}",
        String::from_utf8_lossy(&started.stderr),
        sandbox.daemon_log()
    );
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("unknown service: odd"),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(
        sandbox.daemon_log().contains("remain_after_exit"),
        "the load warning must explain the rejection:\n{}",
        sandbox.daemon_log()
    );
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
    sandbox.start_daemon_ready();

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

#[test]
fn a_script_without_actions_runs_from_the_top() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\necho top>>\"%~dp0top.txt\"\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho top >> \"$(dirname \"$0\")/top.txt\"\nwhile :; do sleep 1; done\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let marker = sandbox.config.join("services.d").join("top.txt");
    wait_for_marker(&marker, "top");

    let status = sandbox.status("demo");
    assert!(status["details"]["probe"].is_null(), "{status}");

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
}

#[test]
fn a_start_only_script_is_stopped_by_killing_it() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\nif /I \"%1\"==\"start\" goto start\r\nexit /b 64\r\n:start\r\necho start>>\"%~dp0start.txt\"\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\ncase \"$1\" in\n  start)\n    echo start >> \"$(dirname \"$0\")/start.txt\"\n    while :; do sleep 1; done\n    ;;\n  *) exit 64 ;;\nesac\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let marker = sandbox.config.join("services.d").join("start.txt");
    wait_for_marker(&marker, "start");

    let began = Instant::now();
    let stopped = sandbox.stop("demo");

    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "a script without a stop action took {:?} to stop",
        began.elapsed()
    );
    assert!(!sandbox.config.join("services.d").join("stop.txt").exists());
}

#[test]
fn status_output_becomes_supplementary_detail() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\nif /I \"%1\"==\"start\" goto start\r\nif /I \"%1\"==\"status\" goto status\r\nexit /b 64\r\n:start\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n:status\r\necho clock detail\r\nexit /b 0\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\ncase \"$1\" in\n  start)\n    while :; do sleep 1; done\n    ;;\n  status)\n    echo clock detail\n    ;;\n  *) exit 64 ;;\nesac\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let status = sandbox.status("demo");

    assert_eq!(status["details"]["probe"], "clock detail");
}

#[test]
fn a_stop_action_still_runs_when_start_is_not_declared() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    let body = "@echo off\r\nif /I \"%1\"==\"stop\" goto stop\r\necho top>>\"%~dp0top.txt\"\r\n:loop\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n:stop\r\necho stop>>\"%~dp0stop.txt\"\r\nexit /b 0\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\ncase \"$1\" in\n  stop)\n    echo stop >> \"$(dirname \"$0\")/stop.txt\"\n    ;;\n  *)\n    echo top >> \"$(dirname \"$0\")/top.txt\"\n    while :; do sleep 1; done\n    ;;\nesac\n";
    sandbox.write_script_body("demo", body);
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let top = sandbox.config.join("services.d").join("top.txt");
    wait_for_marker(&top, "top");

    let stopped = sandbox.stop("demo");

    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    wait_for_marker(&sandbox.config.join("services.d").join("stop.txt"), "stop");
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
}

#[test]
fn a_background_child_keeps_the_service_running_and_stops_with_it() {
    let mut sandbox = Sandbox::new();
    #[cfg(windows)]
    {
        let body = r#"Remove-Item Alias:start -Force -ErrorAction SilentlyContinue
function start {
  $p = Start-Process -WindowStyle Hidden -PassThru -FilePath 'ping' -ArgumentList '-n','60','127.0.0.1'
  Set-Content -Path (Join-Path $PSScriptRoot 'child.pid') -Value $p.Id
}
function status {
  'child alive'
}
$action = $args[0]
if ($action -eq 'start') { start } elseif ($action -eq 'status') { status }
"#;
        sandbox.write_script_as("demo.ps1", body);
    }
    #[cfg(unix)]
    {
        let body = "#!/bin/sh\nstart() {\n  sleep 60 &\n  echo $! > \"$(dirname \"$0\")/child.pid\"\n}\nstatus() { echo 'child alive'; }\ncase \"$1\" in\n  start) start ;;\n  status) status ;;\n  *) exit 64 ;;\nesac\n";
        sandbox.write_script_body("demo", body);
    }
    sandbox.start_daemon();

    let started = sandbox.start_when_ready("demo");
    assert_eq!(
        started.status.code(),
        Some(0),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    let pid_file = sandbox.config.join("services.d").join("child.pid");
    wait_until("the background child pid", Duration::from_secs(10), || {
        fs::read_to_string(&pid_file)
            .map(|text| text.trim().parse::<u32>().is_ok())
            .unwrap_or(false)
    });
    let child_pid: u32 = fs::read_to_string(&pid_file)
        .expect("child pid file")
        .trim()
        .parse()
        .expect("child pid");

    let child = ProcessInstance::capture(child_pid);
    let leader = ProcessInstance::capture(
        sandbox.status("demo")["summary"]["pid"]
            .as_u64()
            .expect("leader pid") as u32,
    );
    wait_until("the script leader to exit", Duration::from_secs(10), || {
        !leader.is_alive()
    });

    let status = sandbox.status("demo");
    assert_eq!(status["summary"]["state"], "running", "{status}");
    assert_eq!(status["details"]["probe"], "child alive");

    let stopped = sandbox.stop("demo");
    assert_eq!(
        stopped.status.code(),
        Some(0),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    assert_eq!(sandbox.status("demo")["summary"]["state"], "stopped");
    wait_until(
        "the background child to die with the service",
        Duration::from_secs(10),
        || !child.is_alive(),
    );
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
    #[cfg(target_os = "linux")]
    {
        // kill -0 also succeeds for a zombie: the process has exited and only
        // awaits reaping. An adopted pid is reaped by whoever spawned it, not
        // by the daemon under test, so "gone" must mean "not running" (#111).
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) else {
            return false;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            return false;
        };
        let state = rest.chars().next().unwrap_or('X');
        state != 'Z' && state != 'X'
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

/// Creation time of `pid`, in the host's own units. A pid alone is not an
/// identity: the OS hands a freed pid to an unrelated process, and a parallel
/// run spawns enough processes to win that lottery (#155). Pid plus creation
/// time is the same pair the daemon validates before adopting a process
/// (`platform::host::process::start_time`).
fn process_start_time(pid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: the handle is closed before returning and every pointer below
        // names a live `FILETIME`.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return None;
            }
            let mut creation: FILETIME = std::mem::zeroed();
            let mut exit: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
            CloseHandle(handle);
            if ok == 0 {
                return None;
            }
            Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Field 22 of /proc/<pid>/stat, the number the runtime record stores.
        let stat = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
        let rest = stat.split_once(") ")?.1;
        rest.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // No cheap creation-time source here. Instance checks then degrade to
        // pid liveness (the pre-#155 answer): weaker, but never a fake "gone".
        let _ = pid;
        None
    }
}

/// One process instance: the pid plus the creation time the OS gave it. The pid
/// is reused, the creation time is not, so this is what "the same process"
/// means when a test asks whether the pre-crash instance is still around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessInstance {
    pid: u32,
    /// The creation time the OS reported when this instance was captured.
    start: Option<u64>,
    /// The pid was already free when this instance was captured.
    gone: bool,
}

impl ProcessInstance {
    /// Capture the instance that owns `pid` right now. Call this before the
    /// process may die: after death there is no creation time left to read.
    fn capture(pid: u32) -> Self {
        match process_start_time(pid) {
            Some(start) => Self {
                pid,
                start: Some(start),
                gone: false,
            },
            // No creation time: either the pid was already free (then the
            // instance is gone for good) or the host refused to describe it.
            None => Self {
                pid,
                start: None,
                gone: !pid_alive(pid),
            },
        }
    }

    /// True only while *this* instance is still running. A pid that now belongs
    /// to another process (different creation time) is not this instance, even
    /// though [`pid_alive`] answers true.
    fn is_alive(self) -> bool {
        // `pid_alive` is the "is it running" gate: Windows keeps a terminated
        // process reachable by handle (and so by pid) until the last handle
        // closes, while tasklist already reports it as gone.
        if self.gone || !pid_alive(self.pid) {
            return false;
        }
        let Some(recorded) = self.start else {
            // The host refused to describe the process when it was captured;
            // pid liveness is the weaker answer that is left.
            return true;
        };
        // Identity known: the same creation time is the same instance. A pid
        // whose creation time is no longer readable is gone, and asking the pid
        // again would race a fast recycle and blame the new owner for the old
        // instance's exit.
        process_start_time(self.pid) == Some(recorded)
    }

    /// True when `other` is a different instance, so a restart did replace this
    /// one. Same pid with no comparable creation time counts as the same
    /// instance, so a missing identity never fakes a restart.
    fn is_replaced_by(self, other: Self) -> bool {
        if self.gone || self.pid != other.pid {
            return true;
        }
        match (self.start, other.start) {
            (Some(old), Some(new)) => old != new,
            _ => false,
        }
    }
}

/// A pid that has been reused by another process is not the pre-crash
/// instance; this is the false red #155 reported ("the pre-crash instance is
/// still alive"). The reuse cannot be forced, so the creation time of a live
/// process is compared against a recorded one that belongs to a different
/// process: `pid_alive` is true and the instance must still read as gone.
#[test]
fn a_reused_pid_is_not_the_old_instance() {
    let (mut child, pid) = spawn_sleeper();
    let live = ProcessInstance::capture(pid);
    assert!(live.is_alive(), "a live child is the instance we captured");

    // A host without a creation time has only the pid to compare, so the reuse
    // case below cannot be stated there.
    #[cfg(any(windows, target_os = "linux"))]
    {
        let start = live.start.expect("a creation time for a live child");
        let reused = ProcessInstance {
            pid,
            start: Some(start.wrapping_add(1)),
            gone: false,
        };
        assert!(pid_alive(pid), "the pid itself is still occupied");
        assert!(
            !reused.is_alive(),
            "a different process holding pid {pid} is not the pre-crash instance"
        );
        assert!(live.is_replaced_by(reused));
    }

    kill_pid(pid);
    let _ = child.wait();
    assert!(!live.is_alive(), "a reaped child is gone");

    // Release the last handle: until then Windows keeps the terminated process
    // (and its pid) reachable. A pid that is already free at capture carries no
    // identity, so anyone who takes it later is not the instance the test meant
    // to watch.
    drop(child);
    wait_until("the pid to be freed", Duration::from_secs(5), || {
        process_start_time(pid).is_none() && !pid_alive(pid)
    });
    let absent = ProcessInstance::capture(pid);
    assert!(absent.gone, "a free pid must capture as gone: {absent:?}");
    assert!(!absent.is_alive());
}

#[cfg(windows)]
fn spawn_sleeper() -> (Child, u32) {
    let child = Command::new("cmd")
        .args(["/c", "ping", "-n", "30", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleeper");
    let pid = child.id();
    (child, pid)
}

#[cfg(unix)]
fn spawn_sleeper() -> (Child, u32) {
    let child = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleeper");
    let pid = child.id();
    (child, pid)
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

    /// Add or replace the `restart` policy of an already written definition.
    fn set_restart(&self, id: &str, policy: &str) {
        self.set_definition_field(id, "restart", json!(policy));
    }

    /// Add or replace one field of an already written definition.
    fn set_definition_field(&self, id: &str, field: &str, value: Value) {
        let path = self.config.join("services.d").join(format!("{}.json", id));
        let mut definition: Value =
            serde_json::from_slice(&fs::read(&path).expect("read definition")).expect("json");
        definition[field] = value;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&definition).expect("definition"),
        )
        .expect("write definition");
    }

    /// A one-shot service: a process that exits 0 immediately, kept `running`
    /// with no pid (RemainAfterExit).
    fn write_one_shot(&self, id: &str) {
        let (path, args) = if cfg!(windows) {
            (r"C:\Windows\System32\cmd.exe", json!(["/c", "exit", "0"]))
        } else {
            ("/bin/true", json!([]))
        };
        let definition = json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "one shot",
            "protocol": 1,
            "exec": {"path": path, "args": args, "ready": "exec"},
            "restart": "never",
            "remain_after_exit": true
        });
        fs::write(
            self.config.join("services.d").join(format!("{}.json", id)),
            serde_json::to_vec_pretty(&definition).expect("definition"),
        )
        .expect("write definition");
    }

    /// A stop/cleanup action that records that it ran.
    fn write_hook_script(&self, name: &str, marker: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let path = self.root.join(format!("{}.cmd", name));
            fs::write(
                &path,
                format!("@echo off\r\necho stop>>\"{}\"\r\n", marker.display()),
            )
            .expect("write hook script");
            path
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = self.root.join(format!("{}.sh", name));
            fs::write(
                &path,
                format!("#!/bin/sh\necho stop >> \"{}\"\n", marker.display()),
            )
            .expect("write hook script");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod hook script");
            path
        }
    }

    /// Provision the host compatibility id the way `svc compat init` does.
    fn write_device_marker(&self, device: &str) {
        fs::write(
            self.config.join("compat.conf"),
            format!("device = {}\n", device),
        )
        .expect("write compat marker");
    }

    /// A schema-3 plugin restricted to the given device ids.
    fn write_compat_package(&self, id: &str, devices: &[&str]) -> PathBuf {
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
        let interpreter = if cfg!(windows) {
            json!({"name": "powershell", "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]})
        } else {
            json!({"name": "sh"})
        };
        let manifest = json!({
            "schema": 3,
            "name": id,
            "version": "1.0.0",
            "desc": "device restricted plugin",
            "platform": target(),
            "protocol": 1,
            "compat": {"devices": devices},
            "exec": {"path": exec_path, "args": [], "ready": "notify"},
            "interpreter": interpreter,
            "capabilities": [
                {"name": "echo", "desc": "echo", "inputSchema": {"type": "object"}}
            ]
        });
        let package = signed_package(&key, manifest, &files);
        let path = self.root.join(format!("{}.spk", id));
        fs::write(&path, package).expect("write spk");
        path
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
        support::wait_for_state(
            id,
            expected,
            timeout,
            || self.status(id),
            || support::tail_lines(&self.daemon_log(), 20),
        )
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

    /// Wait until a *different instance* runs. The pid alone is not enough: the
    /// OS can hand the pre-crash pid to the new process (which would look like
    /// "not restarted" here) or to any unrelated process (#155).
    fn wait_for_restart(&self, id: &str, old: ProcessInstance, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status(id);
            let pid = status["summary"]["pid"].as_u64().unwrap_or(0);
            if pid > 0 && status["summary"]["state"].as_str() == Some("running") {
                let current = ProcessInstance::capture(pid as u32);
                if old.is_replaced_by(current) {
                    return status;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} did not restart after pid {} (start {:?}): {}",
                id,
                old.pid,
                old.start,
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
    let instance = ProcessInstance::capture(pid as u32);
    assert!(pid > 0);
    assert_eq!(running["summary"]["kind"], kind, "{running}");
    assert_eq!(running["summary"]["enabled"], true, "{running}");
    assert_eq!(running["details"]["restart"], "on-failure", "{running}");
    assert_eq!(running["details"]["failures"], 0, "{running}");
    assert_eq!(running["summary"]["state"], "running");

    // Start is idempotent while running: same instance, not a replacement. Pid
    // equality alone would miss a replacement that won the pid back.
    let again = sandbox.start_when_ready(id);
    assert_eq!(again.status.code(), Some(0));
    let same = sandbox.status(id);
    assert_eq!(same["summary"]["state"], "running");
    let current = ProcessInstance::capture(same["summary"]["pid"].as_u64().expect("pid") as u32);
    assert!(
        !instance.is_replaced_by(current),
        "start while running replaced the instance: {same}"
    );

    // Stop is synchronous, idempotent, and reaps the process.
    let stopped = sandbox.stop_service(id);
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert!(
        !instance.is_alive(),
        "stopped instance {:?} is still alive",
        instance
    );
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
    let old = ProcessInstance::capture(running["summary"]["pid"].as_u64().expect("pid") as u32);

    kill_pid(old.pid);
    let backoff = sandbox.wait_for_state(id, "backoff", Duration::from_secs(5));
    assert!(
        backoff["details"]["failures"].as_u64().unwrap_or(0) >= 1,
        "{backoff}"
    );
    assert!(!backoff["details"]["next_at"].is_null(), "{backoff}");

    let restarted = sandbox.wait_for_restart(id, old, Duration::from_secs(15));
    let current =
        ProcessInstance::capture(restarted["summary"]["pid"].as_u64().expect("pid") as u32);
    assert!(
        old.is_replaced_by(current),
        "{}: the crash restart kept the pre-crash instance: {restarted}",
        id
    );
    assert_eq!(restarted["summary"]["state"], "running");
    sandbox.stop_service(id);
}

#[test]
fn state_matrix_for_core_json_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon_ready();
    assert_state_matrix(&sandbox, "core", "core", true);
}

#[test]
fn state_matrix_for_script_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon_ready();
    assert_state_matrix(&sandbox, "script", "core", true);
}

#[test]
fn state_matrix_for_plugin_service() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    // A fresh install is disabled; opt in to autostart so the matrix matches
    // the enabled core services (the install default has its own test).
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "plug"])
            .status
            .code(),
        Some(0)
    );
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

    let mut instance = ProcessInstance::capture(sandbox.running_pid("crashy"));
    for attempt in 1..=5 {
        kill_pid(instance.pid);
        if attempt < 5 {
            let restarted = sandbox.wait_for_restart("crashy", instance, Duration::from_secs(20));
            instance =
                ProcessInstance::capture(restarted["summary"]["pid"].as_u64().expect("pid") as u32);
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
fn disable_keeps_the_crash_restart_policy() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("off");
    sandbox.start_daemon();
    assert_eq!(
        sandbox.run_when_ready(&["disable", "off"]).status.code(),
        Some(0)
    );

    let instance = ProcessInstance::capture(sandbox.running_pid("off"));
    kill_pid(instance.pid);
    let backoff = sandbox.wait_for_state("off", "backoff", Duration::from_secs(5));
    assert!(
        backoff["details"]["failures"].as_u64().unwrap_or(0) >= 1,
        "{backoff}"
    );

    // enabled only gates the boot start; the default on-failure policy still
    // brings the process back.
    let restarted = sandbox.wait_for_restart("off", instance, Duration::from_secs(20));
    assert_eq!(restarted["summary"]["enabled"], false, "{restarted}");
    sandbox.stop_service("off");
}

#[test]
fn a_never_policy_service_stops_without_restarting() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("once");
    sandbox.set_restart("once", "never");
    sandbox.start_daemon_ready();

    let running = sandbox.wait_for_state("once", "running", Duration::from_secs(15));
    assert_eq!(running["details"]["restart"], "never", "{running}");
    let instance =
        ProcessInstance::capture(running["summary"]["pid"].as_u64().expect("pid") as u32);
    kill_pid(instance.pid);

    let stopped = sandbox.wait_for_state("once", "stopped", Duration::from_secs(10));
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert_eq!(stopped["details"]["failures"], 0, "{stopped}");
    assert!(!instance.is_alive(), "{instance:?} is still alive");
}

#[test]
fn graceful_svcd_restart_boots_enabled_services_and_skips_disabled() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.write_stop_script("script");
    sandbox.write_core_json("off");
    fs::write(sandbox.config.join("services.d").join("off.disabled"), b"")
        .expect("disabled marker");

    sandbox.start_daemon_ready();
    let package = sandbox.write_plugin_package("plug");
    sandbox.install_plugin("plug", &package);
    // Fresh installs are disabled; boot-start requires the opt-in.
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "plug"])
            .status
            .code(),
        Some(0)
    );

    let mut instances = Vec::new();
    for id in ["core", "script", "plug"] {
        instances.push(ProcessInstance::capture(sandbox.running_pid(id)));
    }

    sandbox.shutdown();
    for instance in &instances {
        wait_until(
            &format!("instance {:?} to exit", instance),
            Duration::from_secs(10),
            || !instance.is_alive(),
        );
    }

    sandbox.start_daemon_ready();
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
    sandbox.start_daemon_ready();

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

#[test]
fn an_install_for_another_device_is_rejected() {
    let mut sandbox = Sandbox::new();
    sandbox.write_device_marker("sr6110");
    let package = sandbox.write_compat_package("plug", &["sr1010"]);
    sandbox.start_daemon();

    let output = sandbox.run_when_ready_plugin(&["install", package.to_str().expect("path")]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires device"), "{stderr}");

    let list = sandbox.run_when_ready_plugin(&["list"]);
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains("plug"),
        "rejected package leaked into the list"
    );
}

#[test]
fn an_install_for_this_device_is_accepted() {
    let mut sandbox = Sandbox::new();
    sandbox.write_device_marker("sr6110");
    let package = sandbox.write_compat_package("plug", &["sr6110", "sr1010-v1"]);
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);

    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["kind"], "plugin", "{status}");
    assert_eq!(status["summary"]["enabled"], false, "{status}");
}

#[test]
fn a_universal_package_installs_without_a_marker() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_eq!(sandbox.status("plug")["summary"]["kind"], "plugin");
}

#[test]
fn a_marker_change_marks_an_installed_plugin_failed() {
    let mut sandbox = Sandbox::new();
    sandbox.write_device_marker("sr6110");
    let package = sandbox.write_compat_package("plug", &["sr6110"]);
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.install_plugin("plug", &package);
    sandbox.shutdown();

    // The firmware changes generation under the same installation.
    sandbox.write_device_marker("sr1010");
    sandbox.start_daemon();
    // The reload probe also proves a reload does not prune the plugin from the
    // registry: the status below would say "unknown service" if it did.
    sandbox.wait_for_daemon();

    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["state"], "failed", "{status}");
    assert!(status["summary"]["pid"].is_null(), "{status}");

    let started = sandbox.run_when_ready(&["start", "plug"]);
    assert_eq!(started.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&started.stderr).contains("requires device"),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
}

#[test]
fn a_fresh_plugin_install_is_disabled_until_enabled() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);

    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["enabled"], false, "{status}");
    assert_eq!(status["summary"]["state"], "stopped", "{status}");

    // `plugin enable` flips the boot flag without starting the process.
    let enabled = sandbox.run_when_ready_plugin(&["enable", "plug"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["enabled"], true, "{status}");
    assert_eq!(status["summary"]["state"], "stopped", "{status}");

    let disabled = sandbox.run_when_ready_plugin(&["disable", "plug"]);
    assert_eq!(
        disabled.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );
    assert_eq!(sandbox.status("plug")["summary"]["enabled"], false);

    // The plugin entry refuses core services.
    sandbox.write_core_json("core");
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "core"])
            .status
            .code(),
        Some(1)
    );
}

/// A reload reconciles `services.d` and the plugin store. Carrying the
/// scanned plugins into `desired` is what keeps the stale sweep from
/// unregistering every installed plugin on each reload.
#[test]
fn reload_keeps_an_installed_plugin_registered() {
    let mut sandbox = Sandbox::new();
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    sandbox.install_plugin("plug", &package);

    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = sandbox.status("plug");
    assert_eq!(status["summary"]["state"], "stopped", "{status}");
}

#[test]
fn reload_picks_up_definitions_written_after_boot() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    // Wait for the boot scan to finish before adding the definition. A
    // `svc list` probe would answer from disk before the daemon is ready.
    sandbox.wait_for_daemon();
    sandbox.write_core_json("late");

    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("added: late"), "reload output: {}", text);

    let status = sandbox.wait_for_state("late", "running", Duration::from_secs(15));
    assert_eq!(status["summary"]["enabled"], true, "{status}");
    sandbox.stop_service("late");
}

#[test]
fn reload_stops_and_forgets_a_deleted_definition() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("gone");
    sandbox.start_daemon();
    let instance = ProcessInstance::capture(sandbox.running_pid("gone"));

    fs::remove_file(sandbox.config.join("services.d").join("gone.json"))
        .expect("remove definition");
    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("removed: gone"), "reload output: {}", text);

    wait_until(
        "the removed service to exit",
        Duration::from_secs(15),
        || !instance.is_alive(),
    );
    let status = sandbox.run_when_ready(&["status", "gone"]);
    assert_eq!(status.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&status.stderr).contains("unknown service"));
}

#[test]
fn reload_without_changes_reports_nothing_to_do() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();

    let output = sandbox.run_when_ready(&["reload"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("no changes"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_one_shot_service_stays_running_after_it_finishes() {
    let mut sandbox = Sandbox::new();
    sandbox.write_one_shot("once");
    sandbox.start_daemon_ready();

    // Boot starts it. Once the process exits 0 the service stays running with
    // no pid instead of flipping to stopped.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running"
            && status["summary"]["pid"].is_null()
            && status["details"]["last_exit"] == json!(0)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one-shot never settled into running/pid-null: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Starting again is allowed and returns to the same finished state.
    let again = sandbox.start_when_ready("once");
    assert_eq!(
        again.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running" && status["summary"]["pid"].is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the re-run never settled into running/pid-null: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Stop only runs the stop action (none here) and settles immediately.
    let stopped = sandbox.stop_service("once");
    assert_eq!(stopped["summary"]["state"], "stopped", "{stopped}");
    assert!(stopped["summary"]["pid"].is_null(), "{stopped}");
    assert_ne!(stopped["summary"]["state"], "failed");
}

#[test]
fn a_finished_one_shot_dependency_is_not_rerun() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);
    sandbox.write_service("app", &["once"], Script::Ready);

    sandbox.start_daemon_ready();
    // The one-shot may be observed in its brief Running-with-pid window right
    // after spawn; the point of this test is that it settles to
    // running/pid-null exactly once before the dependent app starts.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running" && status["summary"]["pid"].is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the one-shot never settled to running/pid-null: {status}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    sandbox.wait_for_state("app", "running", Duration::from_secs(15));
    wait_until(
        "the one-shot body to run once",
        Duration::from_secs(5),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 1)
                .unwrap_or(false)
        },
    );

    // Starting the dependent again must not re-execute the finished one-shot.
    sandbox.stop_service("app");
    sandbox.start_service("app");
    std::thread::sleep(Duration::from_millis(200));
    let runs = fs::read_to_string(&marker).expect("marker").lines().count();
    assert_eq!(runs, 1, "the one-shot dependency ran again");
    sandbox.stop_service("app");
}

#[test]
fn restart_reruns_a_finished_one_shot() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);

    sandbox.start_daemon();
    wait_until("the first run", Duration::from_secs(15), || {
        let output = sandbox.run_svc(&["status", "once", "--json"]);
        if output.status.code() != Some(0) {
            return false;
        }
        let settled = serde_json::from_slice::<Value>(&output.stdout)
            .map(|value| value["summary"]["pid"].is_null())
            .unwrap_or(false);
        settled
            && fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 1)
                .unwrap_or(false)
    });

    let output = sandbox.run_when_ready(&["restart", "once"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until("the re-run", Duration::from_secs(15), || {
        let output = sandbox.run_svc(&["status", "once", "--json"]);
        if output.status.code() != Some(0) {
            return false;
        }
        let settled = serde_json::from_slice::<Value>(&output.stdout)
            .map(|value| value["summary"]["pid"].is_null())
            .unwrap_or(false);
        settled
            && fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 2)
                .unwrap_or(false)
    });
    let status = sandbox.status("once");
    assert_eq!(status["summary"]["state"], "running", "{status}");
    assert!(status["summary"]["pid"].is_null(), "{status}");
}

#[test]
fn a_finished_one_shot_is_not_rerun_after_a_daemon_restart() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);

    sandbox.start_daemon_ready();
    let record = sandbox.data.join("run").join("once.json");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        let recorded_done = fs::read_to_string(&record)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|value| value["state"] == "running" && value["pid"].is_null())
            .unwrap_or(false);
        if status["summary"]["state"] == "running"
            && status["summary"]["pid"].is_null()
            && recorded_done
            && fs::read_to_string(&marker)
                .map(|text| text.lines().count() == 1)
                .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one-shot never finished: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    sandbox.force_kill_daemon();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let status = sandbox.wait_for_state("once", "running", Duration::from_secs(10));
    assert!(status["summary"]["pid"].is_null(), "{status}");
    std::thread::sleep(Duration::from_millis(200));
    let runs = fs::read_to_string(&marker).expect("marker").lines().count();
    assert_eq!(runs, 1, "the finished one-shot was executed again");
}

#[test]
fn stopping_a_finished_one_shot_runs_its_stop_action() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("once-stop.txt");
    #[cfg(windows)]
    let hook = sandbox.root.join("once-stop.cmd");
    #[cfg(unix)]
    let hook = sandbox.root.join("once-stop.sh");
    #[cfg(windows)]
    fs::write(&hook, "@echo off\r\necho stop>>\"%~dp0once-stop.txt\"\r\n").expect("stop hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            &hook,
            format!("#!/bin/sh\necho stop >> \"{}\"\n", marker.display()),
        )
        .expect("stop hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod stop hook");
    }
    let (path, args) = if cfg!(windows) {
        (r"C:\Windows\System32\cmd.exe", json!(["/c", "exit", "0"]))
    } else {
        ("/bin/true", json!([]))
    };
    let definition = json!({
        "schema": 1,
        "id": "once",
        "version": "1.0.0",
        "desc": "one shot with cleanup",
        "protocol": 1,
        "exec": {"path": path, "args": args, "ready": "exec"},
        "restart": "never",
        "remain_after_exit": true,
        "lifecycle": {"stop": {"path": hook.to_string_lossy(), "args": []}}
    });
    fs::write(
        sandbox.config.join("services.d").join("once.json"),
        serde_json::to_vec_pretty(&definition).expect("definition"),
    )
    .expect("write definition");

    sandbox.start_daemon_ready();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = sandbox.status("once");
        if status["summary"]["state"] == "running" && status["summary"]["pid"].is_null() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "one-shot never finished: {}",
            status
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let stopped = sandbox.stop_service("once");
    assert_eq!(stopped["summary"]["state"], "stopped", "{stopped}");
    wait_until("the stop action", Duration::from_secs(10), || {
        fs::read_to_string(&marker)
            .map(|text| text.contains("stop"))
            .unwrap_or(false)
    });
}

#[test]
fn a_runtime_record_for_a_deleted_definition_is_dropped_on_start() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("orphan");
    sandbox.start_daemon();
    sandbox.running_pid("orphan");

    let record = sandbox.data.join("run").join("orphan.json");
    wait_until("the runtime record", Duration::from_secs(10), || {
        record.is_file()
    });

    sandbox.force_kill_daemon();
    fs::remove_file(sandbox.config.join("services.d").join("orphan.json"))
        .expect("remove definition");

    sandbox.start_daemon();
    sandbox.wait_for_daemon();
    wait_until(
        "the orphan record to be dropped",
        Duration::from_secs(10),
        || !record.exists(),
    );
}

#[test]
fn restart_runs_stop_then_start_and_changes_the_pid() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();
    let first = ProcessInstance::capture(sandbox.running_pid("core"));

    let output = sandbox.run_when_ready(&["restart", "core"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let status = sandbox.wait_for_state("core", "running", Duration::from_secs(15));
    let second = ProcessInstance::capture(status["summary"]["pid"].as_u64().expect("pid") as u32);
    // `restart` must replace the process. Pid inequality alone is the wrong
    // check: the OS may hand the old pid to the new process, and an unrelated
    // process may hold the old pid without being the old instance (#155).
    assert!(
        first.is_replaced_by(second),
        "restart kept the pre-crash instance: {status}"
    );
    // The old process exits asynchronously after `restart` returns. Asserting
    // `!pid_alive` immediately made a parallel full run red while the process
    // was still winding down (#153); wait for the observable end state instead.
    wait_until(
        "the pre-restart instance to exit",
        Duration::from_secs(10),
        || !first.is_alive(),
    );
    sandbox.stop_service("core");
}

/// A failed attempt may have created half its resources; the stop action must
/// run before the retry (shell `svcd` does `run_stop` before the retry sleep).
#[test]
fn a_failed_attempt_runs_the_stop_action_before_retrying() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("fail-stop.txt");
    let hook = sandbox.write_hook_script("fail-stop", &marker);
    let exec = sandbox.write_crash_after_ready_script("failer");
    let definition = json!({
        "schema": 1,
        "id": "failer",
        "version": "1.0.0",
        "desc": "crashes after ready",
        "protocol": 1,
        "exec": exec_for(&exec),
        "restart": "on-failure",
        "lifecycle": {"stop": {"path": hook.to_string_lossy(), "args": []}}
    });
    fs::write(
        sandbox.config.join("services.d").join("failer.json"),
        serde_json::to_vec_pretty(&definition).expect("definition"),
    )
    .expect("write definition");

    sandbox.start_daemon();
    wait_until("the retry cleanup", Duration::from_secs(15), || {
        fs::read_to_string(&marker)
            .map(|text| text.contains("stop"))
            .unwrap_or(false)
    });
}

/// A record caught in `stopping` is finished with the service's stop action,
/// not just force-killed.
#[cfg(unix)]
#[test]
fn a_stopping_record_is_finished_with_the_stop_action() {
    use std::os::unix::process::CommandExt;

    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("half-stop.txt");
    let hook = sandbox.write_hook_script("half-stop", &marker);
    let definition = json!({
        "schema": 1,
        "id": "half",
        "version": "1.0.0",
        "desc": "interrupted stop",
        "protocol": 1,
        "exec": {"path": "/bin/true", "args": [], "ready": "exec"},
        "restart": "never",
        "lifecycle": {"stop": {"path": hook.to_string_lossy(), "args": []}}
    });
    fs::write(
        sandbox.config.join("services.d").join("half.json"),
        serde_json::to_vec_pretty(&definition).expect("definition"),
    )
    .expect("write definition");
    fs::write(sandbox.config.join("services.d").join("half.disabled"), b"")
        .expect("disable at boot");

    let mut child = Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .expect("spawn leftover");
    let pid = child.id();
    let leftover = ProcessInstance::capture(pid);
    // The record stores the same field 22 that the instance identity captured.
    let starttime: u64 = leftover.start.expect("starttime");
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("boot id")
        .trim()
        .to_string();
    let run_dir = sandbox.data.join("run");
    fs::create_dir_all(&run_dir).expect("run dir");
    fs::write(
        run_dir.join("half.json"),
        serde_json::to_vec(&json!({
            "boot": boot,
            "state": "stopping",
            "pid": pid,
            "starttime": starttime,
            "since": 1u64,
        }))
        .expect("record"),
    )
    .expect("write record");

    sandbox.start_daemon();
    wait_until(
        "the stop action for the interrupted stop",
        Duration::from_secs(15),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.contains("stop"))
                .unwrap_or(false)
        },
    );
    wait_until(
        "the leftover process to go away",
        Duration::from_secs(10),
        || !leftover.is_alive(),
    );
    let _ = child.wait();
}

#[test]
fn a_definition_added_while_running_is_picked_up_without_reload() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_with_env(&[("SVCD_RESCAN_INTERVAL_MS", "50")]);
    sandbox.wait_for_daemon();

    sandbox.write_core_json("late");
    wait_until(
        "the new definition to start",
        Duration::from_secs(15),
        || {
            let output = sandbox.run_svc(&["status", "late", "--json"]);
            if output.status.code() != Some(0) {
                return false;
            }
            serde_json::from_slice::<Value>(&output.stdout)
                .map(|value| value["summary"]["state"] == "running")
                .unwrap_or(false)
        },
    );
    let status = sandbox.status("late");
    assert_eq!(status["summary"]["enabled"], true, "{status}");
    sandbox.stop_service("late");
}

#[test]
fn a_definition_deleted_while_running_is_stopped_without_reload() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("gone");
    sandbox.start_daemon_with_env(&[("SVCD_RESCAN_INTERVAL_MS", "50")]);
    let instance = ProcessInstance::capture(sandbox.running_pid("gone"));

    fs::remove_file(sandbox.config.join("services.d").join("gone.json"))
        .expect("remove definition");
    wait_until(
        "the removed service to disappear",
        Duration::from_secs(15),
        || sandbox.run_svc(&["status", "gone"]).status.code() == Some(1) && !instance.is_alive(),
    );
}

#[test]
fn a_graceful_shutdown_runs_the_stop_action() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.root.join("shutdown-stop.txt");
    let hook = sandbox.write_hook_script("shutdown-stop", &marker);
    sandbox.write_core_json("script");
    let path = sandbox.config.join("services.d").join("script.json");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&path).expect("definition")).expect("json");
    value["lifecycle"] = json!({"stop": {"path": hook.to_string_lossy(), "args": []}});
    fs::write(&path, serde_json::to_vec_pretty(&value).expect("json")).expect("write definition");

    sandbox.start_daemon();
    sandbox.running_pid("script");
    sandbox.shutdown();

    wait_until("the shutdown cleanup", Duration::from_secs(10), || {
        fs::read_to_string(&marker)
            .map(|text| text.contains("stop"))
            .unwrap_or(false)
    });
}

#[test]
fn a_finished_one_shot_is_rerun_after_a_graceful_restart() {
    let mut sandbox = Sandbox::new();
    let marker = sandbox.config.join("services.d").join("once-marker.txt");
    #[cfg(windows)]
    let body = "@echo off\r\necho once>>\"%~dp0once-marker.txt\"\r\n";
    #[cfg(unix)]
    let body = "#!/bin/sh\necho once >> \"$(dirname \"$0\")/once-marker.txt\"\n";
    sandbox.write_script_body("once", body);

    sandbox.start_daemon();
    wait_until("the one-shot to run", Duration::from_secs(15), || {
        fs::read_to_string(&marker)
            .map(|text| text.lines().count() >= 1)
            .unwrap_or(false)
    });
    sandbox.shutdown();
    sandbox.start_daemon();
    wait_until(
        "the one-shot to run again after restart",
        Duration::from_secs(15),
        || {
            fs::read_to_string(&marker)
                .map(|text| text.lines().count() >= 2)
                .unwrap_or(false)
        },
    );
}

#[test]
fn a_second_daemon_refuses_to_start() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    sandbox.wait_for_daemon();

    let mut command = Command::new(&sandbox.svcd);
    command
        .args(["run", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sandbox.env(&mut command);
    let mut second = command.spawn().expect("spawn second daemon");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = second.try_wait().expect("try_wait") {
            assert_eq!(
                status.code(),
                Some(1),
                "second daemon should refuse to start"
            );
            return;
        }
        assert!(Instant::now() < deadline, "the second daemon did not exit");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// On Windows the daemon-wide job object (`KILL_ON_JOB_CLOSE`) reaps every
/// child when svcd dies, so the restarted daemon starts exactly one fresh
/// instance per service. Unix deliberately does the opposite and adopts the
/// survivors; `a_restarted_daemon_adopts_an_exec_ready_service` covers that.
#[cfg(windows)]
#[test]
fn abrupt_svcd_exit_reaps_children_and_restart_starts_one_instance() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.write_stop_script("script");
    let package = sandbox.write_plugin_package("plug");
    sandbox.start_daemon();
    sandbox.install_plugin("plug", &package);
    assert_eq!(
        sandbox
            .run_when_ready_plugin(&["enable", "plug"])
            .status
            .code(),
        Some(0)
    );

    let core = ProcessInstance::capture(sandbox.running_pid("core"));
    let script = ProcessInstance::capture(sandbox.running_pid("script"));
    let plugin = ProcessInstance::capture(sandbox.running_pid("plug"));

    sandbox.force_kill_daemon();

    wait_until(
        "all children to die with svcd",
        Duration::from_secs(10),
        || !core.is_alive() && !script.is_alive() && !plugin.is_alive(),
    );

    sandbox.start_daemon();
    for (id, old) in [("core", core), ("script", script), ("plug", plugin)] {
        let status = sandbox.wait_for_restart(id, old, Duration::from_secs(15));
        let current =
            ProcessInstance::capture(status["summary"]["pid"].as_u64().unwrap_or(0) as u32);
        assert!(current.pid > 0, "{status}");
        // The pre-crash instance must be gone. The pid alone cannot say that:
        // under a parallel run the OS hands the freed pid to an unrelated
        // process quickly, so compare the instance identity instead (#155).
        assert!(
            !old.is_alive(),
            "{}: the pre-crash instance (pid {}, start {:?}) is still alive after {:?}",
            id,
            old.pid,
            old.start,
            current
        );
        assert!(
            old.is_replaced_by(current),
            "{}: the restarted daemon adopted the pre-crash instance: {status}",
            id
        );
    }

    sandbox.stop_service("core");
    sandbox.stop_service("script");
    sandbox.stop_service("plug");
}

/// Unix keeps the supervised tree alive when svcd is killed and the next daemon
/// reconnects to it from the runtime record: same pid, no duplicate instance.
#[cfg(unix)]
#[test]
fn a_restarted_daemon_adopts_an_exec_ready_service() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon_ready();
    let adopted = ProcessInstance::capture(sandbox.running_pid("script"));

    sandbox.force_kill_daemon();
    wait_until(
        "the service to survive svcd",
        Duration::from_secs(10),
        || adopted.is_alive(),
    );

    sandbox.start_daemon_ready();
    let status = sandbox.wait_for_state("script", "running", Duration::from_secs(15));
    assert_eq!(
        status["summary"]["pid"].as_u64(),
        Some(adopted.pid as u64),
        "the restarted daemon should adopt pid {}: {}",
        adopted.pid,
        status
    );
    assert_eq!(status["details"]["failures"], 0, "{status}");
    sandbox.stop_service("script");
}

/// A record from another boot must never be adopted: pid numbers restart and a
/// stale starttime could match an unrelated process.
#[cfg(unix)]
#[test]
fn a_runtime_record_from_another_boot_is_ignored() {
    let mut sandbox = Sandbox::new();
    sandbox.write_stop_script("script");
    sandbox.start_daemon_ready();
    let stale = ProcessInstance::capture(sandbox.running_pid("script"));
    sandbox.shutdown();

    // Leave a record as if the previous daemon had written it before a reboot.
    let run_dir = sandbox.data.join("run");
    fs::create_dir_all(&run_dir).expect("run dir");
    fs::write(
        run_dir.join("script.json"),
        serde_json::to_vec(&json!({
            "boot": "an-older-boot",
            "state": "running",
            "pid": stale.pid,
        }))
        .expect("json"),
    )
    .expect("rewrite record");

    sandbox.start_daemon_ready();
    let status = sandbox.wait_for_state("script", "running", Duration::from_secs(15));
    let current = ProcessInstance::capture(status["summary"]["pid"].as_u64().expect("pid") as u32);
    assert!(
        stale.is_replaced_by(current),
        "a record from another boot must not be adopted: {}",
        status
    );
    sandbox.stop_service("script");
}

/// An HTTP bind failure must disable only HTTP. The control channel owns the
/// server loop now, so the CLI keeps working on the same daemon.
#[test]
fn a_busy_http_port_keeps_the_control_channel_alive() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().expect("occupied port").port();
    let bind = format!("127.0.0.1:{}", port);
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon_with_env(&[("SVCD_BIND", bind.as_str())]);
    // reload has no offline fallback; answering it proves the control loop is up.
    sandbox.wait_for_daemon();
    sandbox.shutdown();
}

/// The live text view carries both the summary and the details block.
#[test]
fn status_text_shows_summary_and_details() {
    let mut sandbox = Sandbox::new();
    sandbox.write_core_json("core");
    sandbox.start_daemon();

    let output = sandbox.run_when_ready(&["status", "core"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "summary:",
        "details:",
        "id:",
        "kind:",
        "state:",
        "dependencies:",
        "failures:",
        "restart:",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    assert!(text.contains("core"), "service id missing:\n{text}");
    // The daemon starts an enabled core service, so the state is `running`
    // here; only its presence matters to the text layout.
    assert!(
        text.contains("running") || text.contains("stopped"),
        "state missing:\n{text}"
    );
}

/// #161: the daemon writes what it derived at startup, and `svc status
/// --resources` shows it back. The resource description is injected through the
/// document, so the assertions do not depend on the machine running the suite;
/// two over-the-band values produce exactly two readable overrides.
#[test]
fn the_daemon_reports_the_memory_budget_it_derived() {
    let mut sandbox = Sandbox::new();
    // 8 GiB of free memory -> a 2 GiB budget (the build ceiling), i.e. the
    // streaming shape. The two explicit values are above that shape's share.
    fs::write(
        sandbox.config.join("svcd.conf"),
        "probe_available_memory = 8589934592\nmax_connections = 4096\nmax_body_bytes = 1073741824\n",
    )
    .expect("write svcd.conf");
    sandbox.start_daemon_ready();

    let snapshot = sandbox
        .data
        .join("run")
        .join("resources")
        .join("startup.json");
    let text = wait_for_file(&snapshot, || sandbox.daemon_log());
    let report: Value = serde_json::from_str(&text).expect("resource snapshot");
    for key in [
        "budget_raw",
        "budget_quantized",
        "budget_source",
        "build_cap",
        "workload",
        "workload_source",
        "worst_case",
        "notes",
    ] {
        assert!(report.get(key).is_some(), "snapshot misses {key}: {text}");
    }
    assert_eq!(report["budget_quantized"], 2u64 * 1024 * 1024 * 1024);
    assert_eq!(report["workload"], "streaming");
    assert!(
        report["worst_case"].as_u64().unwrap() <= report["budget_quantized"].as_u64().unwrap(),
        "the derivation must fit its own budget: {text}"
    );

    // Exactly the two rewrites, with their reason and both numbers.
    let notes = report["notes"].as_array().expect("notes");
    let summary: Vec<String> = notes
        .iter()
        .map(|note| {
            format!(
                "{} {} {} {}",
                note["key"].as_str().unwrap_or("?"),
                note["configured"],
                note["effective"],
                note["reason"].as_str().unwrap_or("?")
            )
        })
        .collect();
    assert_eq!(notes.len(), 2, "{summary:?}");
    assert!(
        summary
            .iter()
            .any(|line| line.starts_with("max_connections 4096 ")
                && line.ends_with("above-upper-bound")),
        "{summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|line| line.starts_with("max_body_bytes 1073741824 ")
                && line.ends_with("above-upper-bound")),
        "{summary:?}"
    );

    let output = sandbox.run_svc(&["status", "--resources"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "source:  last start",
        "memory:",
        "workload: streaming (derived)",
        "worst case:",
        "override: max_connections: 4096 ->",
        "override: max_body_bytes: 1073741824 ->",
        "stated:  probe_available_memory",
    ] {
        assert!(shown.contains(needle), "missing {needle} in:\n{shown}");
    }

    // The same report is in the startup log, once.
    let log = sandbox.daemon_log();
    assert!(
        log.contains("svcd: memory:"),
        "log misses the budget:\n{log}"
    );
    assert!(
        log.contains("svcd: workload: streaming (derived)"),
        "log misses the shape:\n{log}"
    );
    let overrides = log.matches("svcd: override: max_connections:").count();
    assert_eq!(overrides, 1, "each override is logged once:\n{log}");
}

/// The snapshot appears after the daemon has logged; ask for it with a bound
/// and print the daemon log when the wait runs out.
fn wait_for_file(path: &Path, log: impl Fn() -> String) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return text;
        }
        if Instant::now() >= deadline {
            panic!("{} never appeared\ndaemon log:\n{}", path.display(), log());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
