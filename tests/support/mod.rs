//! Shared plumbing for the integration sandboxes.
//!
//! Declared with `mod support;` in the test crates that need it. Cargo does not
//! discover a `tests/support/` directory as a test target, so this file is only
//! compiled into the crates that ask for it.

// Each test crate uses a subset of these helpers.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// One sandbox's directories and multicall entry points.
///
/// Every suite needs the same four roots and a `svcd`/`svc`/`plugin`/`mcp`
/// set linked to the cargo-built binary; the extra entries are harmless when a
/// suite only dispatches through two of them.
pub struct SandboxDirs {
    pub root: PathBuf,
    pub config: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
    pub trust: PathBuf,
    pub svcd: PathBuf,
    pub svc: PathBuf,
    pub plugin: PathBuf,
    pub mcp: PathBuf,
    pub daemon_stderr: PathBuf,
}

pub fn sandbox_root(prefix: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = sandbox_base().join(format!(
        "svcd-{}-{}-{}-{}",
        prefix,
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("create sandbox root");
    root
}

pub fn sandbox_dirs(prefix: &str) -> SandboxDirs {
    let root = sandbox_root(prefix);
    let config = root.join("config");
    let data = root.join("data");
    let logs = root.join("logs");
    let trust = config.join("trust.d");
    std::fs::create_dir_all(config.join("services.d")).expect("create sandbox");
    std::fs::create_dir_all(&data).expect("create data");
    std::fs::create_dir_all(&logs).expect("create logs");
    std::fs::create_dir_all(&trust).expect("create trust.d");

    let binary = env!("CARGO_BIN_EXE_svcd");
    let (svcd_name, svc_name, plugin_name, mcp_name) = if cfg!(windows) {
        ("svcd.exe", "svc.exe", "plugin.exe", "mcp.exe")
    } else {
        ("svcd", "svc", "plugin", "mcp")
    };
    let svcd = root.join(svcd_name);
    let svc = root.join(svc_name);
    let plugin = root.join(plugin_name);
    let mcp = root.join(mcp_name);
    install_multicall(Path::new(binary), &[&svcd, &svc, &plugin, &mcp]);

    SandboxDirs {
        daemon_stderr: root.join("daemon.stderr"),
        root,
        config,
        data,
        logs,
        trust,
        svcd,
        svc,
        plugin,
        mcp,
    }
}

/// The root every integration sandbox creates its unique directory under.
///
/// Unix domain sockets cap the whole path at ~108 bytes, so Unix keeps the
/// short system temp. Windows needs a target-local directory: a restricted
/// `%TEMP%` ACL made sandbox creation fail there.
pub fn sandbox_base() -> PathBuf {
    if cfg!(windows) {
        return std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")));
    }
    std::env::temp_dir()
}

/// The platform string the running test binary was built for. Package
/// fixtures declare it so install refuses a foreign target on every host.
pub fn target() -> &'static str {
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

/// How long a sandbox waits for its daemon to stop after a shutdown request.
/// A request that never reaches the daemon must not hang the suite.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Install the multicall entry points a sandbox runs (`svcd` / `svc` / ...).
///
/// The dispatch reads only argv[0]'s file name, so Unix links the entries to the
/// cargo-built binary: a copy that is exec'd right after it is written races its
/// own close on slow filesystems (/mnt/d, DrvFs) and fails with ETXTBSY.
/// Windows uses a hardlink: creating a symlink needs a privilege there, but a
/// hardlink does not, and it keeps one image on disk instead of three copies.
/// A filesystem without hardlinks (or a target dir on another volume) falls
/// back to a copy.
///
/// On Unix a symlink makes `current_exe()` resolve to the cargo-built binary;
/// on Windows the entry is a hardlink, so `current_exe()` keeps the sandbox
/// path. None of the sandboxed suites self-installs — that path lives in
/// `install.rs` and is covered by the suites that exec the cargo binary
/// directly — so only argv[0] dispatch is under test here.
pub fn install_multicall(binary: &Path, entries: &[&Path]) {
    for path in entries {
        #[cfg(unix)]
        std::os::unix::fs::symlink(binary, path).expect("link multicall");
        #[cfg(not(unix))]
        link_or_copy_with(binary, path, |source, destination| {
            std::fs::hard_link(source, destination)
        })
        .expect("link or copy multicall");
    }
}

/// Link `binary` at `path`, falling back to a copy. Split from
/// [`install_multicall`] so the fallback can be tested with an injected
/// failure instead of relying on a filesystem that cannot hardlink.
pub(crate) fn link_or_copy_with(
    binary: &Path,
    path: &Path,
    link: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    link(binary, path).or_else(|_| std::fs::copy(binary, path).map(|_| ()))
}

/// Run one shutdown request with a bounded wait. `Command::output()` has none:
/// the request waits for a control reply, so a daemon that accepts but never
/// answers would block the sandbox `Drop` the same way an unbounded
/// `child.wait()` did.
pub fn run_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run the shutdown request: {error}"))?;
    if let Err(error) = wait_for_exit(&mut child, "the shutdown request", timeout) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    child
        .wait_with_output()
        .map_err(|error| format!("cannot read the shutdown response: {error}"))
}

/// Wait for a process to exit, bounded. The caller kills and reaps on `Err`.
pub fn wait_for_exit(child: &mut Child, what: &str, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => return Err(format!("cannot wait for {what}: {error}")),
        }
        if Instant::now() >= deadline {
            return Err(format!("{what} did not exit within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Stop a daemon: send the shutdown request with a bound, wait for the exit
/// with the same bound, and kill + reap when the wait runs out. The two bounds
/// are sequential, so the worst case is two `timeout`s (a request that hangs
/// plus a daemon that will not exit). `Err` carries the request outcome and the
/// daemon log tail so a failure explains itself.
pub fn stop_daemon(
    child: &mut Child,
    command: &mut Command,
    timeout: Duration,
    daemon_log: impl FnOnce() -> String,
) -> Result<(), String> {
    let request = run_bounded(command, timeout);
    match wait_for_exit(child, "the daemon", timeout) {
        Ok(()) => {
            // A failed request next to a clean exit is a race with the exit (the
            // reply can be lost on the way out), not a failure: keep it visible.
            if !matches!(&request, Ok(output) if output.status.success()) {
                eprintln!(
                    "sandbox: the shutdown request reported {} but the daemon exited",
                    failed_request(&request)
                );
            }
            Ok(())
        }
        Err(error) => {
            // The handle is already out of the sandbox, so leaving the process
            // running here would leak it past the Drop.
            let _ = child.kill();
            let _ = child.wait();
            Err(format!(
                "{error}; killed it. shutdown request: {}; daemon stderr (tail):\n{}",
                failed_request(&request),
                tail_lines(&daemon_log(), 20)
            ))
        }
    }
}

/// Stop a sandbox daemon and assert the wait was bounded and explained itself.
/// A daemon whose control channel is gone must be killed, not waited on.
pub fn assert_bounded_stop(stop: impl FnOnce() -> Result<(), String>, timeout: Duration) {
    let started = Instant::now();
    let error = stop().expect_err("a daemon that had to be killed must be reported");
    assert!(
        started.elapsed() < timeout * 4,
        "the stop wait must be bounded, took {:?}",
        started.elapsed()
    );
    assert!(
        error.contains("did not exit within"),
        "the error must name the wait: {error}"
    );
    assert!(
        error.contains("shutdown request:"),
        "the error must carry the request outcome: {error}"
    );
    assert!(
        error.contains("daemon stderr (tail):"),
        "the error must carry the daemon log: {error}"
    );
}

/// The `Drop` wrapper for [`stop_daemon`]: a panic while another panic unwinds
/// aborts the process and hides the failure that started it, so report instead.
pub fn report_stop(result: Result<(), String>) {
    match result {
        Ok(()) => {}
        Err(message) if std::thread::panicking() => eprintln!("sandbox: {message}"),
        Err(message) => panic!("sandbox: {message}"),
    }
}

/// What the shutdown request looked like, for diagnostics.
pub fn failed_request(outcome: &Result<Output, String>) -> String {
    match outcome {
        Ok(output) => format!(
            "exit {:?} stderr {:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(message) => message.clone(),
    }
}

/// Last `lines` lines of a log, for a failure message.
pub fn tail_lines(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Poll a service's status until `summary.state` is `expected`, bounded.
///
/// `probe` is the sandbox's own status call; `details` runs only on timeout, so
/// a failed wait names the daemon log instead of only the state mismatch. The
/// daemon and plugin sandboxes share this; `tests/http.rs` has its own
/// `wait_until` with a different shape on purpose.
pub fn wait_for_state(
    id: &str,
    expected: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Value,
    details: impl FnOnce() -> String,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let status = probe();
        if status["summary"]["state"].as_str() == Some(expected) {
            return status;
        }
        if Instant::now() >= deadline {
            panic!(
                "service {id} never reached {expected} within {timeout:?}: {status}\ndaemon log (tail):\n{}",
                details()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// True while `pid` is a running process.
///
/// `kill -0` also succeeds for a zombie: the process has exited and only awaits
/// reaping. An adopted pid is reaped by whoever spawned it, not by the daemon
/// under test, so "gone" must mean "not running" (#111).
pub fn pid_alive(pid: u32) -> bool {
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
