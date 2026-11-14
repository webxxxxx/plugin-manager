use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{json, Value};

use crate::ipc::{self, FrameError, FrameEvent};
use crate::log::{self, LogSink};
use crate::paths;
#[cfg(unix)]
use crate::reactor::Framing;
use crate::registry::{Exec, Hook, Interpreter, Lifecycle, Ready, State};
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
/// Bounded per-plugin stdout queue. A full queue pauses that plugin's reader
/// instead of growing memory without bound.
pub(crate) const FRAME_QUEUE: usize = 256;

/// One supervised service: the pure state machine plus the OS handles and
/// deadlines the machine's actions drive.
pub(crate) struct Slot {
    id: String,
    pub(crate) machine: Machine,
    exec: Exec,
    protocol: u32,
    lifecycle: Option<Lifecycle>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames: Option<Receiver<FrameEvent>>,
    wake: Option<Arc<Wake>>,
    #[cfg(unix)]
    reactor: Option<Arc<crate::reactor::Reactor>>,
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
    /// The whole service tree on Windows; on Unix the process group is the tree.
    #[cfg(windows)]
    job: Option<crate::proc::Job>,
    /// The leader exited but other group members are still alive; the service
    /// stays `Running` until the whole group is gone.
    pending_exit: Option<Option<i32>>,
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
    progress_token: Option<Value>,
    reply: crate::channel::Reply,
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
            protocol: 1,
            lifecycle,
            child: None,
            stdin: None,
            frames: None,
            wake: None,
            #[cfg(unix)]
            reactor: None,
            log: None,
            protocol_only: false,
            pending: BTreeMap::new(),
            next_id: 1,
            start_deadline: None,
            stop_deadline: None,
            kill_deadline: None,
            stop_hook: None,
            stop_hook_started: false,
            #[cfg(windows)]
            job: None,
            pending_exit: None,
            ping_id: 1,
        }
    }

    pub(crate) fn set_exec(&mut self, exec: Exec) {
        self.exec = exec;
    }

    pub(crate) fn set_lifecycle(&mut self, lifecycle: Option<Lifecycle>) {
        self.lifecycle = lifecycle;
    }

    pub(crate) fn set_protocol(&mut self, protocol: u32) {
        self.protocol = protocol;
    }

    pub(crate) fn set_protocol_only(&mut self, protocol_only: bool) {
        self.protocol_only = protocol_only;
    }

    pub(crate) fn set_wake(&mut self, wake: Arc<Wake>) {
        self.wake = Some(wake);
    }

    #[cfg(unix)]
    pub(crate) fn set_reactor(&mut self, reactor: Option<Arc<crate::reactor::Reactor>>) {
        self.reactor = reactor;
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
        progress_token: Option<Value>,
        reply: crate::channel::Reply,
    ) -> Result<(), CallError> {
        if self.machine.state != State::Running {
            return Err(CallError::NotLive);
        }
        if self.pending.len() >= MAX_IN_FLIGHT {
            return Err(CallError::Busy);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut params = json!({ "name": capability, "arguments": arguments });
        if let Some(token) = &progress_token {
            params["_meta"] = json!({ "progressToken": token });
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params,
        })
        .to_string();
        if frame.len() > ipc::CALL_FRAME_MAX {
            return Err(CallError::TooLarge);
        }
        if let Err(error) = self.write_json_frame(&frame, ipc::CALL_FRAME_MAX) {
            return Err(CallError::Write(error.to_string()));
        }
        self.pending.insert(
            id,
            PendingCall {
                external_id,
                capability: capability.to_string(),
                deadline: now.saturating_add(CALL_TIMEOUT_MS),
                progress_token,
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
                    let has_stop_hook = self
                        .lifecycle
                        .as_ref()
                        .and_then(|lifecycle| lifecycle.stop.as_ref())
                        .is_some();
                    if !has_stop_hook {
                        self.stop_without_hook();
                    } else if !self.ensure_stop_hook(now) {
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
        self.pending_exit = None;
        #[cfg(windows)]
        if self.job.is_none() {
            self.job = crate::proc::Job::new();
            if self.job.is_none() {
                log::svcd(&format!(
                    "svcd: {}: process job unavailable; stop only terminates the leader",
                    self.id
                ));
            }
        }
        let start = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.start.as_ref());
        let mut command = match start {
            Some(hook) => hook_command(&self.id, "start", hook),
            None => command_for(&self.exec),
        };
        let blob_dir = crate::blob::dir();
        let _ = std::fs::create_dir_all(crate::blob::incoming_dir());
        command.env("SVCD_TEMP_DIR", &blob_dir);
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
        #[cfg(windows)]
        if let Some(job) = &self.job {
            if !job.assign(&child) {
                log::svcd(&format!(
                    "svcd: {}: cannot assign the process job; stop only terminates the leader",
                    self.id
                ));
            }
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

            let (sender, receiver) = mpsc::sync_channel(FRAME_QUEUE);
            #[cfg(unix)]
            if let Some(reactor) = self.reactor.clone() {
                let framing = if self.protocol >= 2 {
                    Framing::Length
                } else {
                    Framing::Lines
                };
                if let Err(error) = reactor.register(
                    self.id.clone(),
                    stdout,
                    Some(stderr),
                    sender,
                    self.wake.clone(),
                    log.clone(),
                    ipc::CALL_FRAME_MAX,
                    framing,
                ) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.to_string());
                }
            } else if let Err(error) = spawn_thread_readers(
                &self.id,
                self.protocol,
                stdout,
                stderr,
                log.clone(),
                sender,
                self.wake.clone(),
            ) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.to_string());
            }
            #[cfg(not(unix))]
            if let Err(error) = spawn_thread_readers(
                &self.id,
                self.protocol,
                stdout,
                stderr,
                log.clone(),
                sender,
                self.wake.clone(),
            ) {
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
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "svcd.ping",
            "params": {},
        })
        .to_string();
        let _ = self.write_json_frame(&line, ipc::CONTROL_FRAME_MAX);
    }

    fn write_json_frame(&mut self, frame: &str, max: usize) -> std::io::Result<()> {
        let protocol = self.protocol;
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "plugin stdin is closed",
            ));
        };
        if protocol >= 2 {
            ipc::write_length_frame_limit(stdin, frame, max)
        } else {
            ipc::write_frame_limit(stdin, frame, max)
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
                FrameEvent::Frame(Err(FrameError::TooLarge { max })) => {
                    self.protocol_violation(&format!("frame exceeds {} bytes", max));
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
        if method == "notifications/progress" {
            if let Some(token) = params.and_then(|params| params.get("progressToken")) {
                let line = json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params.cloned().unwrap_or(Value::Null),
                })
                .to_string();
                for pending in self
                    .pending
                    .values()
                    .filter(|pending| pending.progress_token.as_ref() == Some(token))
                {
                    pending.reply.send(line.clone());
                }
            }
        }
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
            let mut result = result.clone();
            match crate::blob::materialize_result(&mut result) {
                Ok(()) => rpc::ok(pending.external_id, result),
                Err(message) => crate::mcp::tool_error(pending.external_id, &message),
            }
        } else {
            crate::mcp::tool_error(
                pending.external_id,
                "plugin returned neither result nor error",
            )
        };
        pending.reply.send(rpc::to_line(&response));
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
            pending.reply.send(rpc::to_line(&response));
        }
    }

    fn fail_pending(&mut self, reason: &str) {
        let pending = std::mem::take(&mut self.pending);
        for (_, call) in pending {
            let response = crate::mcp::tool_error(call.external_id, reason);
            call.reply.send(rpc::to_line(&response));
        }
    }

    fn write_notification(&mut self, method: &str, params: Value) {
        let frame = rpc::notification(method, params);
        let _ = self.write_json_frame(&frame, ipc::CONTROL_FRAME_MAX);
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
        if let Some(child) = self.child.as_mut() {
            let status = match child.try_wait() {
                Ok(Some(status)) => status,
                _ => return,
            };
            let pid = self.machine.pid;
            self.child = None;
            if pid.is_some() && self.group_alive() {
                // The leader is gone but the service tree is not. Keep the
                // service Running and settle the exit once the group is empty.
                self.pending_exit = Some(status.code());
                return;
            }
            if let Some(pid) = pid {
                self.clear_process();
                self.apply(
                    Event::Exited {
                        pid,
                        code: status.code(),
                    },
                    now,
                );
            }
            return;
        }
        let Some(code) = self.pending_exit else {
            return;
        };
        if self.group_alive() {
            return;
        }
        self.pending_exit = None;
        if let Some(pid) = self.machine.pid {
            self.clear_process();
            self.apply(Event::Exited { pid, code }, now);
        }
    }

    /// True while any process of the service (not just the leader) is alive.
    fn group_alive(&self) -> bool {
        let Some(pid) = self.machine.pid else {
            return false;
        };
        #[cfg(unix)]
        {
            // Signal 0: 0 means a member exists; EPERM means it exists but is
            // not ours to signal, which still counts as alive.
            if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
                return true;
            }
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        }
        #[cfg(windows)]
        {
            let _ = pid;
            self.job
                .as_ref()
                .map(|job| job.active() > 0)
                .unwrap_or(false)
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
                #[cfg(windows)]
                if let Some(job) = &self.job {
                    // The stop action's own children belong to the service tree.
                    job.assign(&child);
                }
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
            // Windows has no polite signal for an exec-ready service. Once the
            // stop hook (the only graceful path) is done, terminate the tree
            // instead of burning the whole stop window and force-killing anyway.
            #[cfg(windows)]
            if self.stdin.is_none() {
                self.signal_force();
                return;
            }
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
        let line = rpc::notification("svcd.shutdown", json!({}));
        let _ = self.write_json_frame(&line, ipc::CONTROL_FRAME_MAX);
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

    /// No `stop` action was declared. Unix already has SIGTERM as the polite
    /// signal; Windows has no signal for an exec-ready script, so kill directly
    /// instead of burning the whole stop window waiting for nothing.
    fn stop_without_hook(&mut self) {
        #[cfg(windows)]
        if self.stdin.is_none() {
            self.signal_force();
            return;
        }
        self.signal_polite();
    }

    pub(crate) fn signal_force(&mut self) {
        self.abort_stop_hook();
        #[cfg(windows)]
        {
            // The leader may already be gone; the job still owns the tree.
            if let Some(job) = &self.job {
                job.terminate();
            }
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
        #[cfg(unix)]
        if let Some(reactor) = &self.reactor {
            reactor.unregister(&self.id);
        }
        self.child = None;
        self.stdin = None;
        self.frames = None;
        self.start_deadline = None;
        self.stop_deadline = None;
        self.kill_deadline = None;
        self.pending_exit = None;
    }
}

fn command_for(exec: &Exec) -> Command {
    match &exec.interpreter {
        Some(interpreter) => {
            command_for_interpreter(interpreter, &exec.path, &exec.args, &exec.env)
        }
        None => command_for_parts(&exec.path, &exec.args, &exec.env),
    }
}

pub(crate) fn hook_command(id: &str, action: &str, hook: &Hook) -> Command {
    let mut command = match &hook.interpreter {
        Some(interpreter) => {
            command_for_interpreter(interpreter, &hook.path, &hook.args, &hook.env)
        }
        None => command_for_parts(&hook.path, &hook.args, &hook.env),
    };
    command
        .env("SVCD_SERVICE_ID", id)
        .env("SVCD_ACTION", action)
        .env("SVCD_DATA_DIR", paths::data_dir())
        .env("SVCD_CONFIG_DIR", paths::config_dir())
        .env("SVCD_LOG_DIR", paths::logs_dir());
    command
}

/// A script payload is not executable on its own in every layout: the manifest
/// resolved an interpreter at install time, so the supervisor runs
/// `interp [interp args] script [action args]`.
fn command_for_interpreter(
    interpreter: &Interpreter,
    script: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Command {
    let mut command = Command::new(&interpreter.path);
    command
        .args(&interpreter.args)
        .arg(script)
        .args(args)
        .envs(env);
    crate::proc::configure(&mut command);
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

/// Fallback used on Windows and whenever the reactor is unavailable.
fn spawn_thread_readers(
    id: &str,
    protocol: u32,
    stdout: ChildStdout,
    stderr: ChildStderr,
    log: Option<Arc<Mutex<LogSink>>>,
    sender: mpsc::SyncSender<FrameEvent>,
    wake: Option<Arc<Wake>>,
) -> std::io::Result<()> {
    if protocol >= 2 {
        ipc::spawn_length_reader_limit(
            format!("svcd-{}-stdout", id),
            stdout,
            sender,
            wake,
            ipc::CALL_FRAME_MAX,
        )?;
    } else {
        ipc::spawn_reader_limit(
            format!("svcd-{}-stdout", id),
            stdout,
            sender,
            wake,
            ipc::CALL_FRAME_MAX,
        )?;
    }
    let stderr_log = log;
    thread::Builder::new()
        .name(format!("svcd-{}-stderr", id))
        .stack_size(threads::IO_STACK)
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                log_line(&stderr_log, &line);
            }
        })?;
    Ok(())
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
            interpreter: None,
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
                progress_token: None,
                reply: crate::channel::Reply::plain(tx),
            },
        );
        rx
    }

    #[test]
    fn an_interpreter_wraps_the_script_and_its_arguments() {
        let exec = Exec {
            path: "clock.sh".into(),
            args: vec!["start".into()],
            env: BTreeMap::new(),
            ready: Ready::Exec,
            interpreter: Some(Interpreter {
                path: "/usr/bin/sh".into(),
                args: vec!["-e".into()],
            }),
        };

        let command = command_for(&exec);
        let args: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect();

        assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/sh");
        assert_eq!(args, vec!["-e", "clock.sh", "start"]);
    }

    #[test]
    fn a_hook_with_an_interpreter_keeps_the_action_environment() {
        let hook = Hook {
            path: "actions.sh".into(),
            args: vec!["stop".into()],
            env: BTreeMap::new(),
            timeout_ms: 1_000,
            interpreter: Some(Interpreter {
                path: "/usr/bin/bash".into(),
                args: Vec::new(),
            }),
        };

        let command = hook_command("demo", "stop", &hook);
        let args: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().to_string())
            .collect();

        assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/bash");
        assert_eq!(args, vec!["actions.sh", "stop"]);
        assert!(command.get_envs().any(|(key, value)| {
            key.to_string_lossy() == "SVCD_ACTION"
                && value
                    .map(|value| value.to_string_lossy() == "stop")
                    .unwrap_or(false)
        }));
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

        let result = slot.request_capability(
            "echo",
            json!({}),
            Some(json!(500)),
            0,
            None,
            crate::channel::Reply::plain(tx),
        );

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
            interpreter: None,
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
        slot.request_capability(
            "echo",
            json!({}),
            Some(json!(100)),
            elapsed(&started),
            None,
            crate::channel::Reply::plain(tx),
        )
        .expect("echo call");
        let response = wait_response(&mut slot, &rx, &started);
        assert_eq!(
            response.result.expect("echo result")["content"][0]["text"],
            "pong"
        );

        let (tx, rx) = mpsc::channel();
        slot.request_capability(
            "fail",
            json!({}),
            Some(json!(101)),
            elapsed(&started),
            None,
            crate::channel::Reply::plain(tx),
        )
        .expect("fail call");
        let result = wait_response(&mut slot, &rx, &started)
            .result
            .expect("failure result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "boom");

        let (tx, rx) = mpsc::channel();
        slot.request_capability(
            "hang",
            json!({}),
            Some(json!(102)),
            elapsed(&started),
            None,
            crate::channel::Reply::plain(tx),
        )
        .expect("hang call");
        let (crash_tx, _crash_rx) = mpsc::channel();
        let _ = slot.request_capability(
            "crash",
            json!({}),
            Some(json!(103)),
            elapsed(&started),
            None,
            crate::channel::Reply::plain(crash_tx),
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
