//! A capability answered by a one-shot action (#342).
//!
//! A plugin with `exec.ready = "exec"` or `"port"` has no protocol channel, so
//! a capability is one process per call: the arguments JSON on stdin, the
//! result JSON on stdout. The work runs on its own short-lived thread — the slot
//! event loop is single threaded and owns every plugin pipe — and hands the
//! outcome straight to the caller's sink.
//!
//! Both pipes go to temp files instead of reader threads: a hook that leaves a
//! grandchild holding stdout must not pin a thread of ours.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use crate::platform::log::Level;

use super::*;

/// How many hook calls one service may have in flight. Each one forks a
/// process, so this is not the channel path's frame count: an unbounded fan-out
/// here would fork the host to death.
pub(super) const MAX_HOOK_CALLS: usize = 4;

/// One call's inputs, moved to its worker thread. Dropping it releases the
/// in-flight slot, whatever the outcome.
pub(super) struct HookCall {
    pub(super) id: String,
    pub(super) hook: Hook,
    pub(super) arguments: Value,
    pub(super) timeout: Duration,
    pub(super) cancel: crate::kernel::contract::CancelToken,
    pub(super) paths: Arc<Paths>,
    pub(super) plugin_dir: Option<PathBuf>,
    pub(super) limits: Arc<crate::platform::limits::Limits>,
    pub(super) inflight: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for HookCall {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Slot {
    /// The hook that answers `capability`, if this service has no channel and
    /// the capability declared one.
    pub(super) fn capability_hook(&self, capability: &str) -> Option<Hook> {
        self.capability_hooks.get(capability).cloned()
    }

    /// Hand one call to a hook worker. The caller's sink runs on that thread,
    /// exactly where the channel path's reply runs.
    pub(super) fn spawn_hook_call(
        &mut self,
        capability: &str,
        hook: Hook,
        invocation: Invocation,
        timeout: Duration,
        sink: ReplySink,
    ) {
        if self.hook_calls.load(Ordering::SeqCst) >= MAX_HOOK_CALLS {
            self.finish_sink(
                sink,
                Err(Failure::new(
                    Class::Busy,
                    Retry::After(std::time::Duration::from_millis(100)),
                    "too many in-flight hook calls",
                )),
            );
            return;
        }
        self.hook_calls.fetch_add(1, Ordering::SeqCst);
        let call = HookCall {
            id: self.id.clone(),
            hook,
            arguments: invocation.arguments,
            timeout,
            cancel: invocation.cancel,
            paths: self.paths.clone(),
            plugin_dir: self.plugin_dir.clone(),
            limits: self.limits.clone(),
            inflight: self.hook_calls.clone(),
        };
        let name = format!("svcd-{}-call-{}", self.id, capability);
        let spawned = std::thread::Builder::new()
            .name(name)
            .stack_size(crate::platform::host::threads::io_stack(&self.limits))
            .spawn(move || {
                sink(run(&call).map(Reply::Value));
            });
        if let Err(error) = spawned {
            self.hook_calls.fetch_sub(1, Ordering::SeqCst);
            error!(
                "{}: cannot start a worker for a hook call: {}",
                self.id, error
            );
        }
    }
}

/// Run one hook call on the worker thread.
fn run(call: &HookCall) -> Result<Value, Failure> {
    let out_path = output_path(&call.id, &call.paths, "out");
    let err_path = output_path(&call.id, &call.paths, "err");
    let (out_file, err_file) = match (fs::File::create(&out_path), fs::File::create(&err_path)) {
        (Ok(out), Ok(err)) => (out, err),
        (out, err) => {
            let detail = format!(
                "cannot create the call's output files: {}",
                out.err()
                    .or_else(|| err.err())
                    .map(|error| error.to_string())
                    .unwrap_or_default()
            );
            let _ = fs::remove_file(&out_path);
            let _ = fs::remove_file(&err_path);
            return Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                detail,
            ));
        }
    };
    let mut command = hook_command(
        &call.id,
        "call",
        &call.hook,
        &call.paths,
        call.plugin_dir.as_deref(),
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&out_path);
            let _ = fs::remove_file(&err_path);
            return Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                format!("cannot start the hook: {error}"),
            ));
        }
    };
    let _ = crate::platform::host::process::adopt(&child);
    // The arguments go in as one JSON object and then stdin closes: a hook that
    // reads to EOF must not wait for a caller who will never write again.
    if let Some(mut stdin) = child.stdin.take() {
        let body = format!("{}\n", call.arguments);
        if let Err(error) = stdin.write_all(body.as_bytes()) {
            crate::platform::host::process::kill_now(&mut child);
            let _ = child.wait();
            let _ = fs::remove_file(&out_path);
            let _ = fs::remove_file(&err_path);
            return Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                format!("cannot send the arguments to the hook: {error}"),
            ));
        }
    }
    let deadline = Instant::now() + call.timeout;
    let end = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(format!("wait failed: {error}")),
        }
        if call.cancel.is_cancelled() {
            break Err("cancelled".into());
        }
        if Instant::now() >= deadline {
            break Err("timeout".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if end.is_err() {
        crate::platform::host::process::kill_now(&mut child);
        let _ = child.wait();
    }
    let output = read_output(&out_path);
    log_stderr(call, &err_path);
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    match end {
        Ok(status) if status.success() => Ok(text_result(&output)),
        Ok(status) => Err(Failure::new(
            Class::ImplementationError,
            Retry::Never,
            format!(
                "the hook exited {}{}",
                status,
                if output.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", clip(output.trim()))
                }
            ),
        )),
        Err(reason) if reason == "cancelled" => Err(Failure::new(
            Class::Cancelled,
            Retry::Never,
            "the call was cancelled",
        )),
        Err(reason) if reason == "timeout" => Err(Failure::new(
            Class::Timeout,
            Retry::After(std::time::Duration::from_secs(1)),
            format!(
                "the hook call timed out after {}ms",
                call.timeout.as_millis()
            ),
        )),
        Err(reason) => Err(Failure::new(
            Class::ImplementationError,
            Retry::Never,
            reason,
        )),
    }
}

/// The hook's stderr belongs in the plugin's hook log, not in the result.
fn log_stderr(call: &HookCall, path: &std::path::Path) {
    let text = read_capped(path, 8 * 1024);
    if text.trim().is_empty() {
        return;
    }
    let Ok(mut log) = LogWriter::open(
        &call.paths.logs_dir,
        &format!("{}-hooks", call.id),
        &format!("hook:{}", call.id),
        &call.limits,
        log::clock(),
        false,
    ) else {
        return;
    };
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let _ = log.record(Level::Info, line);
    }
}

/// A result bigger than a call frame could never be delivered, so refuse it
/// here instead of reading an unbounded file.
fn read_output(path: &std::path::Path) -> String {
    read_capped(path, crate::wire::message::ipc::CALL_FRAME_MAX + 1)
}

fn read_capped(path: &std::path::Path, limit: usize) -> String {
    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = file.take(limit as u64).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).to_string()
}

/// The channel path answers a `tools/call` with the handler's stdout wrapped as
/// one text content part (the shell/PowerShell SDK does the same wrapping on the
/// plugin side). A hook produces exactly that text, so the shape matches and a
/// page's `invoke()` keeps meaning the same thing on both paths.
fn text_result(output: &str) -> Value {
    let text = output.trim_end_matches(['\r', '\n']);
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    })
}

fn clip(text: &str) -> String {
    let mut clipped: String = text.chars().take(160).collect();
    if text.chars().count() > 160 {
        clipped.push('…');
    }
    clipped
}

fn output_path(id: &str, paths: &Paths, kind: &str) -> PathBuf {
    let directory = paths.temp_dir.clone();
    let _ = fs::create_dir_all(&directory);
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    directory.join(format!(
        "call-{}-{}-{}.{}",
        id,
        std::process::id(),
        nanos,
        kind
    ))
}
