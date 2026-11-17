//! Shared plumbing for the integration sandboxes.
//!
//! Declared with `mod support;` in the test crates that need it. Cargo does not
//! discover a `tests/support/` directory as a test target, so this file is only
//! compiled into the crates that ask for it.

// Each test crate uses a subset of these helpers.
#![allow(dead_code)]

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How long a sandbox waits for its daemon to stop after a shutdown request.
/// A request that never reaches the daemon must not hang the suite.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Run one CLI with a bounded wait. `Command::output()` has none: the control
/// request waits for a reply, so a daemon that accepts but never answers would
/// block the sandbox `Drop` the same way the unbounded `child.wait()` did.
pub fn run_bounded(command: &mut Command, what: &str, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {what}: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("cannot read {what} output: {error}"))
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot wait for {what}: {error}"));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what} did not finish within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
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
            return Err(format!("{what} did not stop within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Stop a daemon: send the shutdown request with a bound, wait for the exit
/// with the same bound, and kill + reap when the wait runs out. `Err` carries
/// the request outcome and the daemon log tail so a failure explains itself.
pub fn stop_daemon(
    child: &mut Child,
    command: &mut Command,
    timeout: Duration,
    what: &str,
    daemon_log: impl FnOnce() -> String,
) -> Result<(), String> {
    let request = run_bounded(command, &format!("{what} shutdown request"), timeout);
    match wait_for_exit(child, what, timeout) {
        Ok(()) => {
            // A failed request next to a clean exit is a race with the exit (the
            // reply can be lost on the way out), not a failure: keep it visible.
            if !matches!(&request, Ok(output) if output.status.success()) {
                eprintln!(
                    "sandbox: the shutdown request reported {} but {what} exited",
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

/// Assert that a sandbox's stop wait is bounded: a daemon whose control channel
/// is gone must be killed after `timeout` and reported, not waited on forever.
pub fn assert_bounded_stop(
    child: &mut Child,
    command: &mut Command,
    timeout: Duration,
    daemon_log: impl FnOnce() -> String,
) {
    let started = Instant::now();
    let error = stop_daemon(child, command, timeout, "the daemon", daemon_log)
        .expect_err("a daemon that had to be killed must be reported");
    assert!(
        started.elapsed() < timeout * 4,
        "the stop wait must be bounded, took {:?}",
        started.elapsed()
    );
    assert!(
        error.contains("did not stop"),
        "the error must name the wait: {error}"
    );
    assert!(
        error.contains("shutdown request:"),
        "the error must carry the request outcome: {error}"
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
