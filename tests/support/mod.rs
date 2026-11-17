//! Shared plumbing for the integration sandboxes.
//!
//! Declared with `mod support;` in the test crates that need it. Cargo does not
//! discover a `tests/support/` directory as a test target, so this file is only
//! compiled into the crates that ask for it.

// Each test crate uses a subset of these helpers.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a sandbox waits for its daemon to stop after a shutdown request.
/// A request that never reaches the daemon must not hang the suite.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Install the multicall entry points a sandbox runs (`svcd` / `svc` / ...).
///
/// The dispatch reads only argv[0]'s file name, so Unix links the entries to the
/// cargo-built binary: a copy that is exec'd right after it is written races its
/// own close on slow filesystems (/mnt/d, DrvFs) and fails with ETXTBSY. Windows
/// keeps the copy because creating symlinks needs a privilege there.
///
/// A link makes `current_exe()` resolve to the cargo-built binary instead of the
/// sandbox entry. The integration suites never self-install (that path lives in
/// `install.rs` and is exercised by the suites that keep their own copies), so
/// only argv[0] dispatch is under test.
pub fn install_multicall(binary: &Path, entries: &[&Path]) {
    for path in entries {
        #[cfg(unix)]
        std::os::unix::fs::symlink(binary, path).expect("link multicall");
        #[cfg(not(unix))]
        std::fs::copy(binary, path).expect("copy multicall");
    }
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
