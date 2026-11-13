use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use crate::ipc::{self, FrameError, FrameEvent};
use crate::log::{self, LogSink};
use crate::paths;
use crate::registry::{Exec, Hook, Lifecycle, Ready, State};
use crate::rpc;
use crate::supervisor::{self, Action, Event, Machine};
use crate::threads;
use crate::wake::Wake;

/// Per-plugin in-flight cap from ticket #31. The full spec also describes a
/// 128-slot queue behind this cap; the ticket scopes this tracer bullet to the
/// in-flight cap, so a 65th concurrent call is rejected as busy instead.
pub(crate) const MAX_IN_FLIGHT: usize = 64;
/// A single tool call may not stay in flight longer than this.
pub(crate) const CALL_TIMEOUT_MS: u64 = 30_000;

/// One supervised service: the pure state machine plus the OS handles and
/// deadlines the machine's actions drive.
pub(crate) struct Slot {
    id: String,
    pub(crate) machine: Machine,
    exec: Exec,
    lifecycle: Option<Lifecycle>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames: Option<Receiver<FrameEvent>>,
    wake: Option<Arc<Wake>>,
    log: Option<Arc<Mutex<LogSink>>>,
    /// Plugins must keep stdout protocol-only; core scripts may still print
    /// human-readable output, which is logged for diagnosis.
    protocol_only: bool,
    pending: BTreeMap<u64, PendingCall>,
    next_id: u64,
    start_deadline: Option<u64>,
    stop_deadline: Option<u64>,
    kill_deadline: Option<u64>,
    stop_hook: Option<HookRun>,
    stop_hook_started: bool,
    ping_id: u64,
}

struct HookRun {
    child: Child,
    deadline: u64,
}

struct PendingCall {
    external_id: Option<Value>,
    capability: String,
    deadline: u64,
    reply: Sender<String>,
}

#[derive(Debug)]
pub(crate) enum CallError {
    NotLive,
    Busy,
    TooLarge,
    Write(String),
}

impl Slot {
    pub(crate) fn new(id: String, exec: Exec, lifecycle: Option<Lifecycle>, enabled: bool) -> Self {
        Slot {
            id,
            machine: Machine::new(enabled),
            exec,
            lifecycle,
            child: None,
            stdin: None,
            frames: None,
            wake: None,
            log: None,
            protocol_only: false,
            pending: BTreeMap::new(),
            next_id: 1,
            start_deadline: None,
            stop_deadline: None,
            kill_deadline: None,
            stop_hook: None,
            stop_hook_started: false,
            ping_id: 1,
        }
    }

    pub(crate) fn set_exec(&mut self, exec: Exec) {
        self.exec = exec;
    }

    pub(crate) fn set_lifecycle(&mut self, lifecycle: Option<Lifecycle>) {
        self.lifecycle = lifecycle;
    }

    pub(crate) fn set_protocol_only(&mut self, protocol_only: bool) {
        self.protocol_only = protocol_only;
    }

    pub(crate) fn set_wake(&mut self, wake: Arc<Wake>) {
        self.wake = Some(wake);
    }

    /// Start the stop hook before the state machine begins stopping. The hook
    /// runs in the background; SignalPolite waits for it (or its timeout)
    /// before touching the supervised process.
    pub(crate) fn request_stop(&mut self, now: u64) {
        self.ensure_stop_hook(now);
        self.apply(Event::Stop, now);
    }

    pub(crate) fn pump(&mut self, now: u64) {
        self.drain_frames(now);
        self.check_exit(now);
        self.check_stop_hook(now);
        self.check_timers(now);
    }

    pub(crate) fn apply(&mut self, event: Event, now: u64) {
        let actions = self.machine.step(event, now);
        self.run_actions(actions, now);
    }

    /// Send one tools/call frame and remember the caller until the plugin
    /// answers, times out, or the service goes away.
    pub(crate) fn request_capability(
        &mut self,
        capability: &str,
        arguments: Value,
        external_id: Option<Value>,
        now: u64,
        reply: Sender<String>,
    ) -> Result<(), CallError> {
        if self.machine.state != State::Running {
            return Err(CallError::NotLive);
        }
        if self.pending.len() >= MAX_IN_FLIGHT {
            return Err(CallError::Busy);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": capability, "arguments": arguments },
        })
        .to_string();
        if frame.len() > ipc::MAX_FRAME_BYTES {
            return Err(CallError::TooLarge);
        }
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(CallError::NotLive);
        };
        if let Err(error) = ipc::write_frame(stdin, &frame) {
            return Err(CallError::Write(error.to_string()));
        }
        self.pending.insert(
            id,
            PendingCall {
                external_id,
                capability: capability.to_string(),
                deadline: now.saturating_add(CALL_TIMEOUT_MS),
                reply,
            },
        );
        Ok(())
    }

    fn run_actions(&mut self, actions: Vec<Action>, now: u64) {
        for action in actions {
            match action {
                Action::Spawn => {
                    self.abort_stop_hook();
                    self.stop_hook_started = false;
                    self.fail_pending("service is restarting");
                    match self.spawn() {
                        Ok(pid) => {
                            let actions = self.machine.step(Event::Spawned { pid }, now);
                            self.run_actions(actions, now);
                            if self.exec.ready == Ready::Exec {
                                let actions = self.machine.step(Event::Ready { pid }, now);
                                self.run_actions(actions, now);
                            } else {
                                self.send_ping();
                            }
                        }
                        Err(error) => {
                            log::svcd(&format!("svcd: {}: spawn failed: {}", self.id, error));
                            let actions = self.machine.step(Event::SpawnFailed, now);
                            self.run_actions(actions, now);
                        }
                    }
                }
                Action::ArmStart => {
                    self.start_deadline = Some(now.saturating_add(supervisor::START_TIMEOUT_MS));
                }
                Action::SignalPolite => {
                    if !self.ensure_stop_hook(now) {
                        self.signal_polite();
                    }
                }
                Action::SignalForce => self.signal_force(),
                Action::ArmStop => {
                    self.stop_deadline = Some(now.saturating_add(supervisor::STOP_TIMEOUT_MS));
                }
                Action::ArmKill => {
                    self.kill_deadline = Some(now.saturating_add(supervisor::KILL_TIMEOUT_MS));
                }
                Action::ArmBackoff { .. } => {}
                Action::Publish => {}
                Action::Withdraw => self.fail_pending("service is not available"),
                Action::GiveUp => {
                    self.abort_stop_hook();
                    self.give_up();
                }
            }
        }
    }

    fn spawn(&mut self) -> Result<u32, String> {
        let start = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.start.as_ref());
        let mut command = match start {
            Some(hook) => hook_command(&self.id, "start", hook),
            None => command_for(&self.exec),
        };
        match self.exec.ready {
            Ready::Notify => {
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
            Ready::Exec => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn().map_err(|error| error.to_string())?;
        if let Err(error) = crate::proc::assign(&child) {
            log::svcd(&format!(
                "svcd: {}: parent-death protection: {}",
                self.id, error
            ));
        }
        let pid = child.id();
        let log = match LogSink::open(&paths::logs_dir(), &self.id) {
            Ok(sink) => Some(Arc::new(Mutex::new(sink))),
            Err(error) => {
                log::svcd(&format!(
                    "svcd: {}: cannot open service log: {}",
                    self.id, error
                ));
                None
            }
        };
        self.log = log.clone();

        if self.exec.ready == Ready::Notify {
            let stdin = child.stdin.take();
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stdout is missing".into());
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stderr is missing".into());
                }
            };

            let (sender, receiver) = mpsc::channel();
            if let Err(error) = ipc::spawn_reader(
                format!("svcd-{}-stdout", self.id),
                stdout,
                sender,
                self.wake.clone(),
            ) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }

            let stderr_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stderr", self.id))
                .stack_size(threads::IO_STACK)
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        log_line(&stderr_log, &line);
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }

            self.stdin = stdin;
            self.frames = Some(receiver);
            self.next_id = 1;
            self.ping_id = 1;
        } else {
            self.stdin = None;
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stdout is missing".into());
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("piped stderr is missing".into());
                }
            };

            let stdout_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stdout", self.id))
                .stack_size(threads::IO_STACK)
                .spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines().map_while(Result::ok) {
                        log_line(&stdout_log, &line);
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }

            let stderr_log = log.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("svcd-{}-stderr", self.id))
                .stack_size(threads::IO_STACK)
                .spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().map_while(Result::ok) {
                        log_line(&stderr_log, &line);
                    }
                })
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
            self.frames = None;
        }

        self.child = Some(child);
        Ok(pid)
    }

    fn send_ping(&mut self) {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.ping_id = id;
        if let Some(stdin) = self.stdin.as_mut() {
            let line = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "svcd.ping",
                "params": {},
            })
            .to_string();
            let _ = ipc::write_frame(stdin, &line);
        }
    }

    fn drain_frames(&mut self, now: u64) {
        let mut events = Vec::new();
        if let Some(frames) = &self.frames {
            while let Ok(event) = frames.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                FrameEvent::Frame(Ok(line)) => self.handle_frame(&line, now),
                FrameEvent::Frame(Err(FrameError::TooLarge)) => {
                    self.protocol_violation("frame exceeds 1 MiB");
                    break;
                }
                FrameEvent::Frame(Err(FrameError::InvalidUtf8)) => {
                    self.protocol_violation("frame is not valid UTF-8");
                    break;
                }
                FrameEvent::Frame(Err(FrameError::Io(_))) | FrameEvent::Closed => {
                    self.stdout_closed()
                }
            }
        }
    }

    fn handle_frame(&mut self, line: &str, now: u64) {
        let message = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(message)) => message,
            _ => {
                if self.protocol_only {
                    self.protocol_violation("stdout frame is not a JSON object");
                } else {
                    // Core scripts may print to stdout; keep those lines in the
                    // service log for diagnosis.
                    self.log_service(line);
                }
                return;
            }
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        if let Some(method) = method {
            if id.is_some() {
                self.protocol_violation(
                    "plugin sent a request; only responses and notifications are allowed",
                );
                return;
            }
            self.handle_notification(method, message.get("params"));
            return;
        }
        let Some(id) = id.and_then(Value::as_u64) else {
            if self.protocol_only {
                self.protocol_violation("stdout frame has no method or response id");
            } else {
                self.log_service(line);
            }
            return;
        };
        if self.machine.state == State::Starting && id == self.ping_id {
            if let Some(pid) = self.machine.pid {
                self.apply(Event::Ready { pid }, now);
            }
            return;
        }
        self.complete_pending(id, &message);
    }

    fn handle_notification(&mut self, method: &str, params: Option<&Value>) {
        let text = match method {
            "notifications/message" => params
                .and_then(|params| params.get("data"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| params.map(Value::to_string).unwrap_or_default()),
            "notifications/cancelled" => format!(
                "cancelled: {}",
                params.map(Value::to_string).unwrap_or_default()
            ),
            other => format!(
                "notification {}: {}",
                other,
                params.map(Value::to_string).unwrap_or_default()
            ),
        };
        self.log_service(&text);
        #[cfg(feature = "http")]
        crate::uichannel::publish(&self.id, method, &params.cloned().unwrap_or(Value::Null));
    }

    fn complete_pending(&mut self, id: u64, message: &serde_json::Map<String, Value>) {
        let Some(pending) = self.pending.remove(&id) else {
            log::svcd(&format!(
                "svcd: {}: ignoring response for unknown request {}",
                self.id, id
            ));
            return;
        };
        let response = if let Some(error) = message.get("error") {
            let text = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("plugin call failed");
            crate::mcp::tool_error(pending.external_id, text)
        } else if let Some(result) = message.get("result") {
            rpc::ok(pending.external_id, result.clone())
        } else {
            crate::mcp::tool_error(
                pending.external_id,
                "plugin returned neither result nor error",
            )
        };
        let _ = pending.reply.send(rpc::to_line(&response));
    }

    fn expire_pending(&mut self, now: u64) {
        let expired: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.write_notification(
                "notifications/cancelled",
                json!({ "requestId": id, "reason": "timeout" }),
            );
            let Some(pending) = self.pending.remove(&id) else {
                continue;
            };
            log::svcd(&format!(
                "svcd: {}: tools/call {} timed out",
                self.id, pending.capability
            ));
            let response =
                crate::mcp::tool_error(pending.external_id, "tools/call timed out after 30s");
            let _ = pending.reply.send(rpc::to_line(&response));
        }
    }

    fn fail_pending(&mut self, reason: &str) {
        let pending = std::mem::take(&mut self.pending);
        for (_, call) in pending {
            let response = crate::mcp::tool_error(call.external_id, reason);
            let _ = call.reply.send(rpc::to_line(&response));
        }
    }

    fn write_notification(&mut self, method: &str, params: Value) {
        if let Some(stdin) = self.stdin.as_mut() {
            let frame = rpc::notification(method, params);
            let _ = ipc::write_frame(stdin, &frame);
        }
    }

    /// stdout EOF means the plugin can no longer answer. Treat it as an exit:
    /// force the process down and let the supervisor withdraw its capabilities.
    fn stdout_closed(&mut self) {
        if matches!(self.machine.state, State::Starting | State::Running) {
            log::svcd(&format!(
                "svcd: {}: stdout closed; treating the plugin as exited",
                self.id
            ));
            self.signal_force();
        }
    }

    fn protocol_violation(&mut self, reason: &str) {
        log::svcd(&format!(
            "svcd: {}: protocol violation: {}; killing process",
            self.id, reason
        ));
        self.log_service(&format!("protocol violation: {}", reason));
        self.fail_pending("plugin protocol violation");
        self.signal_force();
    }

    fn log_service(&self, line: &str) {
        log_line(&self.log, line);
    }

    fn check_exit(&mut self, now: u64) {
        let status = self
            .child
            .as_mut()
            .and_then(|child| match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => None,
            });
        if let Some(status) = status {
            let pid = self.machine.pid;
            self.clear_process();
            if let Some(pid) = pid {
                self.apply(
                    Event::Exited {
                        pid,
                        code: status.code(),
                    },
                    now,
                );
            }
        }
    }

    fn check_timers(&mut self, now: u64) {
        self.expire_pending(now);
        if self.machine.state == State::Starting {
            if let Some(deadline) = self.start_deadline {
                if now >= deadline {
                    self.start_deadline = None;
                    self.apply(Event::StartTimeout, now);
                }
            }
        }
        if self.machine.state == State::Stopping {
            if let Some(deadline) = self.stop_deadline {
                if now >= deadline {
                    self.stop_deadline = None;
                    self.apply(Event::StopTimeout, now);
                }
            }
            if let Some(deadline) = self.kill_deadline {
                if now >= deadline {
                    self.kill_deadline = None;
                    self.apply(Event::KillTimeout, now);
                }
            }
        }
        if self.machine.state == State::Backoff {
            if let Some(next_at) = self.machine.next_at_ms {
                if now >= next_at {
                    self.apply(Event::BackoffElapsed, now);
                }
            }
        }
    }

    fn ensure_stop_hook(&mut self, now: u64) -> bool {
        if self.stop_hook.is_some() {
            return true;
        }
        if self.stop_hook_started {
            return false;
        }
        self.stop_hook_started = true;
        let Some(hook) = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.stop.as_ref())
        else {
            return false;
        };
        let mut command = hook_command(&self.id, "stop", hook);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        match command.spawn() {
            Ok(mut child) => {
                let _ = crate::proc::assign(&child);
                let log = LogSink::open(&paths::logs_dir(), &self.id)
                    .ok()
                    .map(|sink| Arc::new(Mutex::new(sink)));
                if let Some(stderr) = child.stderr.take() {
                    let log = log.clone();
                    let id = self.id.clone();
                    let _ = thread::Builder::new()
                        .name(format!("svcd-{}-stop-stderr", id))
                        .stack_size(threads::IO_STACK)
                        .spawn(move || {
                            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                                log_line(&log, &line);
                            }
                        });
                }
                log::svcd(&format!(
                    "svcd: {}: stop hook started ({})",
                    self.id, hook.path
                ));
                self.stop_hook = Some(HookRun {
                    child,
                    deadline: now.saturating_add(hook.timeout_ms),
                });
                true
            }
            Err(error) => {
                log::svcd(&format!(
                    "svcd: {}: stop hook spawn failed: {}",
                    self.id, error
                ));
                false
            }
        }
    }

    fn check_stop_hook(&mut self, now: u64) {
        let Some(run) = self.stop_hook.as_mut() else {
            return;
        };
        let deadline = run.deadline;
        let outcome = match run.child.try_wait() {
            Ok(Some(status)) => Some(Ok(status)),
            Ok(None) if now >= deadline => {
                let _ = run.child.kill();
                let _ = run.child.wait();
                Some(Err("timed out"))
            }
            _ => None,
        };
        let Some(outcome) = outcome else {
            return;
        };
        self.stop_hook = None;
        match outcome {
            Ok(status) => log::svcd(&format!("svcd: {}: stop hook exited {}", self.id, status)),
            Err(reason) => log::svcd(&format!("svcd: {}: stop hook {}", self.id, reason)),
        }
        if self.machine.state == State::Stopping {
            self.signal_polite();
        }
    }

    fn abort_stop_hook(&mut self) {
        if let Some(mut run) = self.stop_hook.take() {
            let _ = run.child.kill();
            let _ = run.child.wait();
            log::svcd(&format!("svcd: {}: stop hook aborted", self.id));
        }
    }

    fn signal_polite(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let line = rpc::notification("svcd.shutdown", json!({}));
            let _ = ipc::write_frame(stdin, &line);
        }
        self.stdin = None;
        #[cfg(unix)]
        {
            if let Some(pid) = self.machine.pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
        }
    }

    pub(crate) fn signal_force(&mut self) {
        self.abort_stop_hook();
        #[cfg(windows)]
        {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
        }
        #[cfg(unix)]
        {
            if let Some(pid) = self.machine.pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }

    fn give_up(&mut self) {
        if self.machine.unkillable {
            log::svcd(&format!("svcd: {}: unkillable after stop timeout", self.id));
        } else {
            log::svcd(&format!(
                "svcd: {}: failed after {} consecutive failures",
                self.id, self.machine.failures
            ));
        }
        self.signal_force();
        self.clear_process();
    }

    fn clear_process(&mut self) {
        self.fail_pending("service exited");
        self.child = None;
        self.stdin = None;
        self.frames = None;
        self.start_deadline = None;
        self.stop_deadline = None;
        self.kill_deadline = None;
    }
}

fn command_for(exec: &Exec) -> Command {
    command_for_parts(&exec.path, &exec.args, &exec.env)
}

pub(crate) fn hook_command(id: &str, action: &str, hook: &Hook) -> Command {
    let mut command = command_for_parts(&hook.path, &hook.args, &hook.env);
    command
        .env("SVCD_SERVICE_ID", id)
        .env("SVCD_ACTION", action)
        .env("SVCD_DATA_DIR", paths::data_dir())
        .env("SVCD_CONFIG_DIR", paths::config_dir())
        .env("SVCD_LOG_DIR", paths::logs_dir());
    command
}

fn command_for_parts(path: &str, args: &[String], env: &BTreeMap<String, String>) -> Command {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "ps1" {
        #[cfg(windows)]
        {
            let mut command = Command::new("powershell");
            command
                .arg("-NoProfile")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-File")
                .arg(path)
                .args(args)
                .envs(env);
            crate::proc::configure(&mut command);
            return command;
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("pwsh");
            command
                .arg("-NoProfile")
                .arg("-File")
                .arg(path)
                .args(args)
                .envs(env);
            crate::proc::configure(&mut command);
            return command;
        }
    }
    #[cfg(windows)]
    if extension == "cmd" || extension == "bat" {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(path).args(args).envs(env);
        crate::proc::configure(&mut command);
        return command;
    }
    let mut command = Command::new(path);
    command.args(args).envs(env);
    crate::proc::configure(&mut command);
    command
}

fn log_line(log: &Option<Arc<Mutex<LogSink>>>, line: &str) {
    if let Some(sink) = log {
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.line(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    fn exec() -> Exec {
        Exec {
            path: "/bin/true".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            ready: Ready::Notify,
        }
    }

    fn slot() -> Slot {
        Slot::new("demo".into(), exec(), None, true)
    }

    fn queue(slot: &mut Slot, id: u64, deadline: u64) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        slot.pending.insert(
            id,
            PendingCall {
                external_id: Some(json!(100 + id)),
                capability: "echo".into(),
                deadline,
                reply: tx,
            },
        );
        rx
    }

    #[test]
    fn notifications_are_mirrored_to_the_ui_bus() {
        let (receiver, subscription) = crate::uichannel::bus()
            .subscribe("slot-ui-test")
            .expect("subscribe");
        let mut slot = Slot::new("slot-ui-test".into(), exec(), None, true);
        slot.handle_notification(
            "notifications/message",
            Some(&json!({"level": "info", "data": "hello"})),
        );
        let event = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("event");
        assert!(
            event.json.contains("notifications/message"),
            "{}",
            event.json
        );
        assert!(event.json.contains("hello"), "{}", event.json);
        drop(subscription);
    }

    #[test]
    fn in_flight_calls_are_capped_at_sixty_four() {
        let mut slot = slot();
        slot.machine.state = State::Running;
        for id in 0..MAX_IN_FLIGHT as u64 {
            let _ = queue(&mut slot, id + 1, u64::MAX);
        }
        let (tx, _rx) = mpsc::channel();

        let result = slot.request_capability("echo", json!({}), Some(json!(500)), 0, tx);

        assert!(matches!(result, Err(CallError::Busy)));
    }

    #[test]
    fn a_timed_out_call_sends_a_cancellation_then_a_tool_error() {
        let mut slot = slot();
        slot.machine.state = State::Running;
        let rx = queue(&mut slot, 7, 10);

        slot.expire_pending(10);

        let line = rx.try_recv().expect("timeout response");
        let response: rpc::Response = serde_json::from_str(&line).expect("json");
        assert_eq!(response.id, Some(json!(107)));
        assert!(response.error.is_none());
        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("timed out"));
        assert!(slot.pending.is_empty());
    }

    #[test]
    fn a_strict_plugin_rejects_non_protocol_stdout() {
        let mut slot = slot();
        slot.set_protocol_only(true);
        slot.machine.state = State::Running;
        let rx = queue(&mut slot, 5, u64::MAX);

        slot.handle_frame("hello from stdout", 0);

        let line = rx.try_recv().expect("failure response");
        let response: rpc::Response = serde_json::from_str(&line).expect("json");
        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        assert!(slot.pending.is_empty());
    }

    #[test]
    fn a_protocol_violation_fails_pending_calls() {
        let mut slot = slot();
        slot.machine.state = State::Running;
        let rx = queue(&mut slot, 5, u64::MAX);

        slot.protocol_violation("frame exceeds 1 MiB");

        let line = rx.try_recv().expect("failure response");
        let response: rpc::Response = serde_json::from_str(&line).expect("json");
        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        assert!(slot.pending.is_empty());
    }

    #[test]
    fn a_crashed_service_marks_pending_calls_as_tool_errors() {
        let mut slot = slot();
        slot.machine.state = State::Running;
        let rx = queue(&mut slot, 3, u64::MAX);

        slot.fail_pending("service exited");

        let line = rx.try_recv().expect("failure response");
        let response: rpc::Response = serde_json::from_str(&line).expect("json");
        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "service exited");
    }

    #[cfg(unix)]
    #[test]
    fn a_plugin_serves_ping_and_tool_calls_then_loses_its_pending_call_on_crash() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("svcd-ipc-unit-{}-{}", std::process::id(), nanos));
        fs::create_dir_all(root.join("logs")).expect("logs");
        std::env::set_var("SVCD_LOG_DIR", root.join("logs"));

        let script = root.join("agent.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *svcd.ping*) printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}
' ;;
    *'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}
' ;;
    *'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"boom"}}
' ;;
    *'"name":"hang"'*) ;;
    *'"name":"crash"'*) exit 0 ;;
  esac
done
"#,
        )
        .expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

        let exec = Exec {
            path: "/bin/sh".into(),
            args: vec![script.to_string_lossy().to_string()],
            env: BTreeMap::new(),
            ready: Ready::Notify,
        };
        let mut slot = Slot::new("demo".into(), exec, None, true);
        slot.set_protocol_only(true);
        let started = Instant::now();
        slot.apply(Event::Start, 0);
        while slot.machine.state != State::Running {
            slot.pump(started.elapsed().as_millis() as u64);
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "plugin did not become ready: {:?}",
                slot.machine.state
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let (tx, rx) = mpsc::channel();
        slot.request_capability("echo", json!({}), Some(json!(100)), elapsed(&started), tx)
            .expect("echo call");
        let response = wait_response(&mut slot, &rx, &started);
        assert_eq!(
            response.result.expect("echo result")["content"][0]["text"],
            "pong"
        );

        let (tx, rx) = mpsc::channel();
        slot.request_capability("fail", json!({}), Some(json!(101)), elapsed(&started), tx)
            .expect("fail call");
        let result = wait_response(&mut slot, &rx, &started)
            .result
            .expect("failure result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "boom");

        let (tx, rx) = mpsc::channel();
        slot.request_capability("hang", json!({}), Some(json!(102)), elapsed(&started), tx)
            .expect("hang call");
        let (crash_tx, _crash_rx) = mpsc::channel();
        let _ = slot.request_capability(
            "crash",
            json!({}),
            Some(json!(103)),
            elapsed(&started),
            crash_tx,
        );

        let result = wait_response(&mut slot, &rx, &started)
            .result
            .expect("crash result");
        assert_eq!(result["isError"], true);
        assert!(slot.pending.is_empty());

        while slot.machine.state == State::Running && started.elapsed() < Duration::from_secs(5) {
            slot.pump(elapsed(&started));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(slot.machine.state, State::Running);

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    fn elapsed(started: &Instant) -> u64 {
        started.elapsed().as_millis() as u64
    }

    #[cfg(unix)]
    fn wait_response(slot: &mut Slot, rx: &Receiver<String>, started: &Instant) -> rpc::Response {
        loop {
            slot.pump(elapsed(started));
            if let Ok(line) = rx.try_recv() {
                return serde_json::from_str(&line).expect("response json");
            }
            assert!(started.elapsed() < Duration::from_secs(5), "no response");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
