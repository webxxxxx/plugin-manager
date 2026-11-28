//! A capability answered by a one-shot action (#342).
//!
//! A plugin with `exec.ready = "exec"` or `"port"` has no protocol channel, so
//! a capability is one process per call: the arguments JSON on stdin, the
//! result text on stdout. The work runs on its own short-lived thread — the slot
//! event loop is single threaded and owns every plugin pipe — and hands the
//! outcome straight to the caller's sink.
//!
//! stdout and stderr go to temp files instead of reader threads: a hook that
//! leaves a grandchild holding stdout must not pin a thread of ours.

use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::platform::log::Level;
use crate::supervise::ops::hook_proc::{self, Capture, Unfinished};

use super::*;

/// How many hook calls one service may have in flight. Each one forks a
/// process, so this is not the channel path's frame count: an unbounded fan-out
/// here would fork the host to death.
pub(super) const MAX_HOOK_CALLS: usize = 4;

/// One call's inputs, moved to its worker thread.
pub(super) struct HookCall {
    pub(super) id: String,
    pub(super) hook: Hook,
    /// The request this call belongs to (`call=N`) and the capability it asks
    /// for; together they make the start line in the hook log (#348).
    pub(super) trace: Option<String>,
    pub(super) capability: String,
    pub(super) arguments: Value,
    pub(super) timeout: Duration,
    pub(super) cancel: crate::kernel::contract::CancelToken,
    pub(super) paths: Arc<Paths>,
    pub(super) plugin_dir: Option<PathBuf>,
    pub(super) limits: Arc<crate::platform::limits::Limits>,
    /// A result may not exceed the contract's inline limit: it is the amount the
    /// MCP face and a page can carry back in one reply. Over that it is a failed
    /// call, not a truncated result that looks like a real answer.
    pub(super) max_result: usize,
}

/// One in-flight call: it holds the caller's sink and one unit of the in-flight
/// budget. If the worker thread never starts, `Drop` does both things by itself —
/// answer the caller and release the unit.
///
/// The sink and the budget live in one `Drop` because they are one ownership:
/// incrementing here and decrementing somewhere else produced exactly the two
/// bugs this shape removes — a double decrement (the budget wrapped, and every
/// later call was refused) and a caller left without any reply.
pub(super) struct Answer {
    sink: Option<ReplySink>,
    inflight: Arc<std::sync::atomic::AtomicUsize>,
}

impl Answer {
    pub(super) fn new(sink: ReplySink, inflight: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Answer {
            sink: Some(sink),
            inflight,
        }
    }

    fn reply(&mut self, outcome: Result<Reply, Failure>) {
        if let Some(sink) = self.sink.take() {
            sink(outcome);
        }
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        self.reply(Err(Failure::new(
            Class::Busy,
            Retry::After(Duration::from_millis(100)),
            "cannot start a worker for the hook call",
        )));
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
                    Retry::After(Duration::from_millis(100)),
                    "too many in-flight hook calls",
                )),
            );
            return;
        }
        self.hook_calls.fetch_add(1, Ordering::SeqCst);
        let call = HookCall {
            id: self.id.clone(),
            hook: hook.clone(),
            trace: invocation.trace,
            capability: capability.to_string(),
            arguments: invocation.arguments,
            // Both budgets apply: the hook declares how long its own action may
            // take, the caller declares how long it will wait.
            timeout: timeout.min(Duration::from_millis(hook.timeout_ms)),
            cancel: invocation.cancel,
            paths: self.paths.clone(),
            plugin_dir: self.plugin_dir.clone(),
            limits: self.limits.clone(),
            max_result: self.max_result(),
        };
        let mut answer = Answer::new(sink, self.hook_calls.clone());
        let name = format!("svcd-{}-call-{}", self.id, capability);
        let spawned = std::thread::Builder::new()
            .name(name)
            .stack_size(crate::platform::host::threads::io_stack(&self.limits))
            .spawn(move || {
                let outcome = run(&call).map(Reply::Value);
                answer.reply(outcome);
            });
        if let Err(error) = spawned {
            // `spawn` dropped the closure, so `Answer::drop` already answered
            // the caller and released the unit; there is nothing left to undo.
            error!(
                "{}: cannot start a worker for a hook call: {}",
                self.id, error
            );
        }
    }

    /// How large a result may be: the contract's inline limit is what a `tools/call`
    /// reply carries on both faces; a scripted wire with no contract falls back to
    /// one call frame.
    fn max_result(&self) -> usize {
        self.contract
            .as_ref()
            .map(|contract| contract.limits.max_inline)
            .unwrap_or(crate::wire::message::ipc::CALL_FRAME_MAX)
    }
}

/// Run one hook call on the worker thread.
pub(super) fn run(call: &HookCall) -> Result<Value, Failure> {
    note_start(call);
    let out = Capture::new(&call.id, "call-out", &call.paths).map_err(|error| {
        implementation(format!("cannot create the call's output file: {error}"))
    })?;
    let err = Capture::new(&call.id, "call-err", &call.paths)
        .map_err(|error| implementation(format!("cannot create the call's error file: {error}")))?;
    let out_file = out
        .open()
        .map_err(|error| implementation(format!("cannot open the call's output file: {error}")))?;
    let err_file = err
        .open()
        .map_err(|error| implementation(format!("cannot open the call's error file: {error}")))?;
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
    let mut child = command
        .spawn()
        .map_err(|error| implementation(format!("cannot start the hook: {error}")))?;
    let _ = crate::platform::host::process::adopt(&child);
    // The arguments go in as one JSON object and then stdin closes: a hook that
    // reads to EOF must not wait for a caller who will never write again.
    if let Some(mut stdin) = child.stdin.take() {
        let body = format!("{}\n", call.arguments);
        if let Err(error) = stdin.write_all(body.as_bytes()) {
            crate::platform::host::process::kill_now(&mut child);
            let _ = child.wait();
            return Err(implementation(format!(
                "cannot send the arguments to the hook: {error}"
            )));
        }
    }
    // No reactor pump here: the hook runs on its own thread, so the slot loop
    // keeps draining plugin stdio while this waits.
    let outcome = hook_proc::wait(
        &mut child,
        call.timeout,
        Some(&call.cancel),
        &mut || {},
        Duration::from_millis(20),
    );
    let (output, overflow) = out.read(call.max_result);
    let (stderr, _) = err.read(8 * 1024);
    log_stderr(call, &stderr);
    match outcome {
        Ok(status) if status.success() => {
            if overflow {
                return Err(Failure::new(
                    Class::FrameTooLarge,
                    Retry::Never,
                    format!(
                        "the hook wrote more than {} bytes; a call result may not exceed the \
                         contract's inline limit",
                        call.max_result
                    ),
                ));
            }
            Ok(text_result(&output))
        }
        Ok(status) => Err(implementation(format!(
            "the hook exited {}{}",
            status,
            if output.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", clip(output.trim()))
            }
        ))),
        Err(Unfinished::Cancelled) => Err(Failure::new(
            Class::Cancelled,
            Retry::Never,
            "the call was cancelled",
        )),
        Err(Unfinished::TimedOut) => Err(Failure::new(
            Class::Timeout,
            Retry::After(Duration::from_secs(1)),
            format!(
                "the hook call timed out after {}ms",
                call.timeout.as_millis()
            ),
        )),
        Err(Unfinished::Failed(reason)) => Err(implementation(reason)),
    }
}

fn implementation(detail: impl Into<String>) -> Failure {
    Failure::new(Class::ImplementationError, Retry::Never, detail)
}

/// One line in the hook log names the request before the hook runs: the hook's
/// own output is appended after it, so two runs of the same hook no longer run
/// together with nothing in between (#348).
fn note_start(call: &HookCall) {
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
    let line = match call.trace.as_deref() {
        Some(trace) => format!("call={} {} start", trace, call.capability),
        None => format!("{} start", call.capability),
    };
    let _ = log.record(Level::Info, &line);
}

/// The hook's stderr belongs in the plugin's hook log, not in the result; stdout
/// is the result and is not logged.
fn log_stderr(call: &HookCall, text: &str) {
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

/// The channel path answers a `tools/call` with the handler's stdout wrapped as
/// one text content part (the shell/PowerShell SDK does the same wrapping on the
/// plugin side). A hook produces exactly that text, so the shape matches and a
/// page's `invoke()` keeps meaning the same thing on both paths. The text is not
/// parsed: whatever the hook printed is the answer.
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
