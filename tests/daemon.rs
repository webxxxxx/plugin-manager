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

#[path = "daemon/adopt.rs"]
mod adopt;
#[path = "daemon/config.rs"]
mod config;
#[path = "daemon/control.rs"]
mod control;
#[path = "daemon/install.rs"]
mod install;
#[path = "daemon/lifecycle.rs"]
mod lifecycle;
#[path = "daemon/reload.rs"]
mod reload;
mod support;
use support::target;

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
    /// Overridable so the bounded-wait test does not spend the production bound.
    stop_timeout: Duration,
}

impl Sandbox {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = support::sandbox_base().join(format!(
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

// ---------------------------------------------------------------------------
// Lifecycle matrix fixtures: all three service sources, crash control and PIDs.
// ---------------------------------------------------------------------------


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
            "schema": 1,
            "id": id,
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
            "id": id,
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
