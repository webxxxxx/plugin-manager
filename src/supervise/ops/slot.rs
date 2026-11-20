use std::collections::BTreeMap;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

#[cfg(test)]
use crate::kernel::contract::ProgressToken;
use crate::kernel::contract::{Contract, Control, Invocation, Reply, ReplySink, Settled, Wire};
use crate::kernel::failure::{Class, Failure, Rejection, Retry};
use crate::kernel::registry::{Exec, Hook, Interpreter, Lifecycle, Ready, Restart, State};
use crate::platform::host::wake::Wake;
use crate::platform::log::{self, LogSink};
use crate::platform::paths::Paths;
use crate::supervise::model::supervisor::{self, Action, Event, Machine};
use crate::wire::contract::stdio::{self, Plan, Streams, Target};
use crate::wire::contract::RAW_PROTOCOL;
use crate::wire::message::ipc::{self, FrameError, FrameEvent};
use crate::wire::message::rpc;

/// Per-plugin in-flight cap from ticket #31. The full spec also describes a
/// 128-slot queue behind this cap; the ticket scopes this tracer bullet to the
/// in-flight cap, so a 65th concurrent call is rejected as busy instead.
pub(crate) const MAX_IN_FLIGHT: usize = crate::kernel::contract::MAX_IN_FLIGHT;
/// Bounded hand-off from a caller's thread to the slot event loop. A full queue
/// is backpressure ('Busy'), not unbounded memory growth.
pub(crate) const CALL_QUEUE: usize = 256;
/// 默认调用超时；生产装配点用 `default_timeout` / `max_timeout` 覆盖它。
pub(crate) const DEFAULT_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Bounded per-plugin stdout queue. A full queue pauses that plugin's reader
/// instead of growing memory without bound.
pub(crate) const FRAME_QUEUE: usize = 256;

/// One supervised service: the pure state machine plus the OS handles and
/// deadlines the machine's actions drive.
type NotifySink = Arc<dyn Fn(&str, &Value) + Send + Sync>;

pub(crate) struct Slot {
    id: String,
    /// The directory set this slot uses for its log, its child environment and
    /// the blob store. Resolved once at the composition root.
    paths: Arc<Paths>,
    limits: Arc<crate::platform::limits::Limits>,
    pub(crate) machine: Machine,
    exec: Exec,
    protocol: u32,
    lifecycle: Option<Lifecycle>,
    child: Option<Child>,
    stdin: Option<stdio::WriteEnd>,
    frames: Option<Receiver<FrameEvent>>,
    wake: Option<Arc<Wake>>,
    /// Who reads this service's stdout/stderr: the supervisor's poll set on
    /// Unix, reader threads elsewhere.
    stdio: Target,
    raw_hub: Option<Arc<crate::wire::contract::rawhub::RawHub>>,
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
    /// The backoff elapsed while the pre-retry stop hook was still running;
    /// spawn as soon as it finishes.
    backoff_pending: bool,
    /// A stable crash is running the stop action; spawn when it finishes.
    cleanup_then_spawn: bool,
    /// The whole service tree; on Unix the process group is the tree and this
    /// stays `None` (the platform returns no handle there).
    tree: Option<crate::platform::host::process::Tree>,
    /// The leader exited but other group members are still alive; the service
    /// stays `Running` until the whole group is gone.
    pending_exit: Option<Option<i32>>,
    ping_id: u64,
    /// This slot was rebuilt from a runtime record after a daemon restart, so
    /// there is no `Child` handle: liveness is polled through the process group.
    adopted: bool,
    /// Optional contract used to validate capabilities and inline limits.
    contract: Option<Contract>,
    /// Default timeout for a call that does not carry its own.
    call_timeout: std::time::Duration,
    /// Face-side projection of a plugin notification (progress/logging).
    notify_sink: Option<NotifySink>,
    /// Requests submitted by callers, drained on the event loop.
    request_tx: mpsc::SyncSender<SlotRequest>,
    request_rx: Receiver<SlotRequest>,
    /// Coarse health snapshot for `SlotHandle::probe`.
    health: Arc<AtomicU8>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        // Dropping the slot means no event loop will ever answer its pending
        // calls. Complete both the in-flight and still-queued ones here instead
        // of dropping the 'FnOnce' sinks and leaving callers waiting forever.
        self.fail_pending("service runtime stopped");
        while let Ok(request) = self.request_rx.try_recv() {
            (request.done)(Err(Failure::new(
                Class::NotRunning,
                Retry::After(std::time::Duration::from_secs(1)),
                "service runtime stopped",
            )));
        }
    }
}

/// A handle to one slot. Submitting never waits: the slot event loop owns the
/// process and calls `done` when the call finishes.
#[derive(Clone)]
pub(crate) struct SlotHandle {
    requests: mpsc::SyncSender<SlotRequest>,
    wake: Option<Arc<Wake>>,
    health: Arc<AtomicU8>,
}

struct SlotRequest {
    invocation: Invocation,
    /// Progress still goes straight to the caller's channel; the final response
    /// goes through `done`.
    client: Option<crate::wire::control::channel::Reply>,
    done: ReplySink,
}

impl SlotHandle {
    /// Submit a call. The event loop drains the request on its next pump and
    /// invokes `done`; the caller keeps running.
    pub(crate) fn submit(
        &self,
        invocation: Invocation,
        client: Option<crate::wire::control::channel::Reply>,
        done: ReplySink,
    ) -> Result<(), (Failure, ReplySink)> {
        let request = SlotRequest {
            invocation,
            client,
            done,
        };
        match self.requests.try_send(request) {
            Ok(()) => {
                if let Some(wake) = &self.wake {
                    wake.notify();
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Full(request)) => Err((
                Failure::new(
                    Class::Busy,
                    Retry::After(std::time::Duration::from_millis(100)),
                    "too many queued calls",
                ),
                request.done,
            )),
            Err(mpsc::TrySendError::Disconnected(request)) => Err((
                Failure::new(
                    Class::NotRunning,
                    Retry::After(std::time::Duration::from_secs(1)),
                    "service runtime is gone",
                ),
                request.done,
            )),
        }
    }

    pub(crate) fn probe(&self) -> Settled {
        match self.health.load(Ordering::SeqCst) {
            1 => Settled::Live,
            2 => Settled::Ready,
            3 => Settled::Degraded("degraded".into()),
            _ => Settled::Stopped,
        }
    }
}

/// The `Wire` used by the daemon. It never touches the process; it submits to
/// the slot event loop and returns, so the daemon itself stays non-blocking.
pub(crate) struct SlotWire {
    handle: SlotHandle,
    client: Option<crate::wire::control::channel::Reply>,
}

impl SlotWire {
    pub(crate) fn new(
        handle: SlotHandle,
        client: Option<crate::wire::control::channel::Reply>,
    ) -> SlotWire {
        SlotWire { handle, client }
    }
}

/// Test-only handle: answers submitted calls with `respond` on its own thread.
/// The thread ends when the handle is dropped, so the conformance suite can build
/// a fresh handle per assertion. It exercises the real `SlotWire` plumbing
/// (submit / done callback) without spawning a supervised process; the event
/// loop's own behaviour stays covered by the slot tests below.
#[cfg(test)]
impl SlotHandle {
    pub(crate) fn scripted(
        respond: impl Fn(&Invocation) -> Result<Value, Failure> + Send + 'static,
    ) -> SlotHandle {
        let (requests, receiver) = mpsc::sync_channel::<SlotRequest>(CALL_QUEUE);
        std::thread::spawn(move || {
            for request in receiver {
                let outcome = respond(&request.invocation);
                (request.done)(outcome.map(Reply::Value));
            }
        });
        SlotHandle {
            requests,
            wake: None,
            // Ready: a delegating adapter must still answer Probe.
            health: Arc::new(AtomicU8::new(2)),
        }
    }
}

impl Wire for SlotWire {
    fn call(&mut self, invocation: Invocation, done: ReplySink) -> Result<(), Rejection> {
        // 内核对「取消后不得返回 Ok」的承诺必须在这一层兑现：submit 之后就没有
        // 取消通道了（事件循环的 deadline 只覆盖超时），一个已取消的调用一旦进槽
        // 就会被真正执行，还可能以 Ok 回到调用方。
        if invocation.cancel.is_cancelled() {
            done(Err(Failure::new(
                Class::Cancelled,
                Retry::Never,
                "call was cancelled before it reached the service",
            )));
            return Ok(());
        }
        if let Err((failure, done)) = self.handle.submit(invocation, self.client.clone(), done) {
            done(Err(failure));
        }
        Ok(())
    }

    fn lifecycle(&mut self, control: Control) -> Result<Settled, Failure> {
        match control {
            Control::Probe => Ok(self.handle.probe()),
            _ => Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                "slot lifecycle must go through the daemon",
            )),
        }
    }
}

struct HookRun {
    child: Child,
    deadline: u64,
}

struct PendingCall {
    capability: String,
    deadline: u64,
    progress_token: Option<String>,
    /// Progress notifications go here while the call is in flight.
    client: Option<crate::wire::control::channel::Reply>,
    /// Final result, delivered to the caller's completion sink. The slot does
    /// not encode it: the face that owns the protocol supplied the sink.
    sink: ReplySink,
}

#[derive(Debug)]
pub(crate) enum CallError {
    Rejected(Rejection),
    Failed(Failure),
}

pub(crate) fn call_error_failure(error: CallError) -> Failure {
    match error {
        CallError::Failed(failure) => failure,
        CallError::Rejected(Rejection::BadArguments { detail }) => {
            Failure::new(Class::FrameTooLarge, Retry::Never, detail)
        }
        CallError::Rejected(Rejection::UnknownCapability { capability }) => Failure::new(
            Class::ImplementationError,
            Retry::Never,
            format!("unknown capability {capability}"),
        ),
    }
}

impl Slot {
    pub(crate) fn new(
        id: String,
        exec: Exec,
        lifecycle: Option<Lifecycle>,
        restart: Restart,
        paths: Arc<Paths>,
        limits: Arc<crate::platform::limits::Limits>,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel(CALL_QUEUE);
        Slot {
            id,
            paths,
            limits,
            machine: Machine::new(restart),
            exec,
            protocol: 1,
            lifecycle,
            child: None,
            stdin: None,
            frames: None,
            wake: None,
            stdio: Target::threaded(),
            raw_hub: None,
            log: None,
            protocol_only: false,
            pending: BTreeMap::new(),
            next_id: 1,
            start_deadline: None,
            stop_deadline: None,
            kill_deadline: None,
            stop_hook: None,
            stop_hook_started: false,
            tree: None,
            pending_exit: None,
            ping_id: 1,
            adopted: false,
            backoff_pending: false,
            cleanup_then_spawn: false,
            contract: None,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            notify_sink: None,
            request_tx,
            request_rx,
            health: Arc::new(AtomicU8::new(0)),
        }
    }

    pub(crate) fn handle(&self) -> SlotHandle {
        SlotHandle {
            requests: self.request_tx.clone(),
            wake: self.wake.clone(),
            health: self.health.clone(),
        }
    }

    pub(crate) fn set_contract(&mut self, contract: Contract) {
        self.contract = Some(contract);
    }

    pub(crate) fn set_call_timeout(&mut self, timeout: std::time::Duration) {
        self.call_timeout = timeout;
    }

    pub(crate) fn set_notifier(&mut self, notify: NotifySink) {
        self.notify_sink = Some(notify);
    }

    pub(crate) fn set_exec(&mut self, exec: Exec) {
        self.exec = exec;
    }

    pub(crate) fn set_lifecycle(&mut self, lifecycle: Option<Lifecycle>) {
        self.lifecycle = lifecycle;
    }

    pub(crate) fn set_restart(&mut self, restart: Restart) {
        self.machine.restart = restart;
    }

    pub(crate) fn set_remain_after_exit(&mut self, remain_after_exit: bool) {
        self.machine.remain_after_exit = remain_after_exit;
    }

    /// Reconnect to a *finished* one-shot: the effect is still there, but there
    /// is no process to validate or adopt. The record only restores the
    /// reported state (so an svcd restart does not re-run the work).
    pub(crate) fn adopt_finished(&mut self, since_ms: u64, last_exit: Option<i32>) {
        self.machine.state = State::Running;
        self.machine.pid = None;
        self.machine.since_ms = since_ms;
        self.machine.ready_since_ms = None;
        self.machine.next_at_ms = None;
        self.machine.last_exit = last_exit;
        self.adopted = false;
        log::svcd(&format!(
            "svcd: {}: restored finished one-shot state (no pid)",
            self.id
        ));
    }

    /// Reconnect this slot to a process that outlived the previous daemon.
    /// `state` is what the record said: a live `running`/`starting` process is
    /// adopted as Running. A record that was `stopping` is adopted the same way
    /// and the caller then runs a normal stop, so the stop action is not lost.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn adopt(
        &mut self,
        pid: u32,
        state: State,
        since_ms: u64,
        ready_since_ms: Option<u64>,
        failures: u32,
        last_exit: Option<i32>,
        now: u64,
    ) {
        let _ = now;
        self.machine.pid = Some(pid);
        self.machine.failures = failures;
        self.machine.last_exit = last_exit;
        self.machine.ready_since_ms = ready_since_ms;
        self.adopted = true;
        match state {
            State::Running | State::Starting | State::Stopping => {
                self.machine.state = State::Running;
                self.machine.since_ms = since_ms;
                log::svcd(&format!("svcd: {}: adopted pid {}", self.id, pid));
            }
            _ => {
                self.machine.pid = None;
                self.adopted = false;
                self.machine.state = State::Stopped;
            }
        }
    }

    pub(crate) fn set_protocol(&mut self, protocol: u32) {
        self.protocol = protocol;
    }

    /// Whether callers depend on this service's stdio channel staying open.
    /// The rule itself lives in `check::peer` so the definition loader and the
    /// runtime cannot drift apart.
    pub(crate) fn is_channel_peer(&self) -> bool {
        crate::supervise::check::peer::is_channel_peer(self.exec.ready, self.protocol)
    }

    /// Seed the state for a service that was already unhealthy at scan time
    /// (for example a plugin whose compat check failed). No process is attached.
    pub(crate) fn set_initial_state(&mut self, state: State) {
        if state == State::Failed {
            self.machine.state = State::Failed;
            self.machine.pid = None;
            self.adopted = false;
        }
    }

    pub(crate) fn set_protocol_only(&mut self, protocol_only: bool) {
        self.protocol_only = protocol_only;
    }

    pub(crate) fn set_wake(&mut self, wake: Arc<Wake>) {
        self.wake = Some(wake);
    }

    /// Adopt the daemon's stdio target. A slot created after the poll set
    /// started gets the poll target; every earlier slot is updated here.
    pub(crate) fn set_stdio(&mut self, stdio: Target) {
        self.stdio = stdio;
    }

    pub(crate) fn set_raw_hub(&mut self, hub: Option<Arc<crate::wire::contract::rawhub::RawHub>>) {
        self.raw_hub = hub;
    }

    /// Start the stop hook before the state machine begins stopping. The hook
    /// runs in the background; SignalPolite waits for it (or its timeout)
    /// before touching the supervised process.
    pub(crate) fn request_stop(&mut self, now: u64) {
        self.ensure_stop_hook(now);
        self.apply(Event::Stop, now);
        // A finished one-shot has no process; without a stop hook there is
        // nothing left to wait for.
        if self.machine.state == State::Stopping
            && self.machine.pid.is_none()
            && self.stop_hook.is_none()
        {
            self.apply(Event::StopSettled, now);
        }
    }

    pub(crate) fn pump(&mut self, now: u64) {
        self.drain_requests(now);
        self.drain_frames(now);
        self.check_exit(now);
        self.check_stop_hook(now);
        self.check_timers(now);
        self.sync_health();
    }

    fn drain_requests(&mut self, now: u64) {
        while let Ok(request) = self.request_rx.try_recv() {
            let mut sink = Some(request.done);
            if let Err(error) =
                self.request_invocation_sink(request.invocation, now, request.client, &mut sink)
            {
                if let Some(sink) = sink.take() {
                    self.finish_sink(sink, Err(call_error_failure(error)));
                }
            }
        }
    }

    fn sync_health(&self) {
        let code = match self.machine.health().state() {
            Settled::Live => 1,
            Settled::Ready => 2,
            Settled::Degraded(_) => 3,
            Settled::Stopped => 0,
        };
        self.health.store(code, Ordering::SeqCst);
    }

    pub(crate) fn apply(&mut self, event: Event, now: u64) {
        let actions = self.machine.step(event, now);
        self.run_actions(actions, now);
    }

    /// Send one tools/call frame and remember the caller until the plugin
    /// answers, times out, or the service goes away.
    #[cfg(test)]
    pub(crate) fn request_capability(
        &mut self,
        capability: &str,
        arguments: Value,
        external_id: Option<Value>,
        now: u64,
        progress_token: Option<Value>,
        reply: crate::wire::control::channel::Reply,
    ) -> Result<(), CallError> {
        let mut invocation = Invocation::new(capability, arguments, self.call_timeout);
        invocation.progress = progress_token.and_then(|token| match token {
            Value::String(token) => Some(ProgressToken::new(token)),
            Value::Number(token) => Some(ProgressToken::new(token.to_string())),
            _ => None,
        });
        self.request_invocation(invocation, external_id, now, reply)
    }

    /// Contract-aware call entry: validation, inline limit, timeout ceiling and
    /// failure classification all go through `kernel`.
    #[cfg(test)]
    pub(crate) fn request_invocation(
        &mut self,
        invocation: Invocation,
        external_id: Option<Value>,
        now: u64,
        reply: crate::wire::control::channel::Reply,
    ) -> Result<(), CallError> {
        let client = Some(reply.clone());
        let mut sink = Some(test_reply_sink(reply, external_id));
        self.request_invocation_sink(invocation, now, client, &mut sink)
    }

    fn request_invocation_sink(
        &mut self,
        invocation: Invocation,
        now: u64,
        client: Option<crate::wire::control::channel::Reply>,
        sink: &mut Option<ReplySink>,
    ) -> Result<(), CallError> {
        if let Some(contract) = &self.contract {
            if contract.capability(&invocation.capability).is_none() {
                return Err(CallError::Rejected(Rejection::UnknownCapability {
                    capability: invocation.capability.as_str().to_string(),
                }));
            }
            if crate::kernel::contract::inline_len(&invocation.arguments)
                > contract.limits.max_inline
            {
                return Err(CallError::Rejected(Rejection::BadArguments {
                    detail: "arguments exceed the contract inline limit".into(),
                }));
            }
        }
        if invocation.timeout.is_zero() {
            return Err(CallError::Rejected(Rejection::BadArguments {
                detail: "timeout must be greater than zero".into(),
            }));
        }
        if self.machine.state != State::Running {
            return Err(CallError::Failed(Failure::new(
                Class::NotRunning,
                Retry::After(std::time::Duration::from_secs(1)),
                format!(
                    "plugin {} is not running; call {}.start first",
                    self.id, self.id
                ),
            )));
        }
        if self.pending.len() >= MAX_IN_FLIGHT {
            return Err(CallError::Failed(Failure::new(
                Class::Busy,
                Retry::After(std::time::Duration::from_millis(100)),
                "too many in-flight tool calls",
            )));
        }
        let timeout = match &self.contract {
            Some(contract) => invocation.timeout.min(contract.limits.max_timeout),
            None => invocation.timeout,
        };
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let mut params = json!({
            "name": invocation.capability.as_str(),
            "arguments": invocation.arguments,
        });
        if let Some(token) = &invocation.progress {
            params["_meta"] = json!({ "progressToken": token.as_str() });
        }
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params,
        })
        .to_string();
        if frame.len() > ipc::CALL_FRAME_MAX {
            return Err(CallError::Rejected(Rejection::BadArguments {
                detail: "tools/call request exceeds the 1 MiB frame limit".into(),
            }));
        }
        if let Err(error) = self.write_json_frame(&frame, ipc::CALL_FRAME_MAX) {
            return Err(CallError::Failed(Failure::new(
                Class::WriteFailed,
                Retry::Never,
                format!("cannot write to plugin: {error}"),
            )));
        }
        let deadline = now.saturating_add(timeout.as_millis().min(u64::MAX as u128) as u64);
        self.pending.insert(
            id,
            PendingCall {
                capability: invocation.capability.as_str().to_string(),
                deadline,
                progress_token: invocation
                    .progress
                    .as_ref()
                    .map(|token| token.as_str().to_string()),
                client,
                sink: sink.take().expect("call sink must be present"),
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
                    self.backoff_pending = false;
                    self.cleanup_then_spawn = false;
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
                Action::SignalForce => self.signal_force("stop timeout"),
                Action::ArmStop => {
                    self.stop_deadline = Some(now.saturating_add(supervisor::STOP_TIMEOUT_MS));
                }
                Action::ArmKill => {
                    self.kill_deadline = Some(now.saturating_add(supervisor::KILL_TIMEOUT_MS));
                }
                // A service that failed may have created half its resources:
                // run the stop action before the next attempt, like the shell
                // `svcd` worker does (`run_stop` before the retry sleep).
                Action::ArmBackoff { .. } => {
                    self.ensure_stop_hook(now);
                }
                // A stable service that just crashed also gets its stop action
                // before the replacement starts.
                Action::Cleanup => {
                    if self.ensure_stop_hook(now) {
                        self.cleanup_then_spawn = true;
                    } else {
                        self.apply(Event::RestartReady, now);
                    }
                }
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
        if self.tree.is_none() {
            self.tree = crate::platform::host::process::Tree::new();
            if self.tree.is_none() {
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
            Some(hook) => service_hook_command(&self.id, "start", hook, &self.paths),
            None => command_for(&self.exec),
        };
        let blob_dir = self.paths.temp_dir.clone();
        let _ = std::fs::create_dir_all(crate::platform::storage::incoming_dir(&self.paths));
        command.env("SVCD_TEMP_DIR", &blob_dir);
        let plan = match self.exec.ready {
            Ready::Notify => Plan::notify(),
            Ready::Exec => Plan::exec(self.protocol),
        };
        let (mut child, streams) =
            stdio::spawn(command, plan).map_err(|error| error.to_string())?;
        if let Err(error) = crate::platform::host::process::adopt(&child) {
            log::svcd(&format!(
                "svcd: {}: parent-death protection: {}",
                self.id, error
            ));
        }
        if let Some(tree) = &self.tree {
            if !tree.assign(&child) {
                log::svcd(&format!(
                    "svcd: {}: cannot assign the process job; stop only terminates the leader",
                    self.id
                ));
            }
        }
        let pid = child.id();
        log::svcd(&format!(
            "svcd: {}: spawned pid {} (ready={:?})",
            self.id, pid, self.exec.ready
        ));
        let log = match LogSink::open(&self.paths.logs_dir, &self.id, &self.limits) {
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

        let Streams {
            stdin,
            stdout,
            stderr,
        } = streams;

        if self.exec.ready == Ready::Notify {
            let (sender, receiver) = mpsc::sync_channel(FRAME_QUEUE);
            let Some(stdout) = stdout else {
                return abandon(&mut child, "piped stdout is missing");
            };
            let Some(stderr) = stderr else {
                return abandon(&mut child, "piped stderr is missing");
            };
            if let Err(error) = self.stdio.frames(
                self.id.clone(),
                self.protocol,
                stdout,
                Some(stderr),
                sender,
                self.wake.clone(),
                log.clone(),
                &self.limits,
            ) {
                return abandon(&mut child, error.to_string());
            }

            self.stdin = stdin;
            self.frames = Some(receiver);
            self.next_id = 1;
            self.ping_id = 1;
        } else {
            self.stdin = None;
            let Some(stdout) = stdout else {
                return abandon(&mut child, "piped stdout is missing");
            };
            let Some(stderr) = stderr else {
                return abandon(&mut child, "piped stderr is missing");
            };

            if self.protocol == RAW_PROTOCOL {
                let Some(stdin) = stdin else {
                    return abandon(&mut child, "piped stdin is missing");
                };
                let Some(hub) = self.raw_hub.clone() else {
                    return abandon(&mut child, "raw transport is unavailable");
                };
                if let Err(error) = hub.register(
                    self.id.clone(),
                    Streams {
                        stdin: Some(stdin),
                        stdout: Some(stdout),
                        stderr: Some(stderr),
                    },
                    log.clone(),
                ) {
                    return abandon(&mut child, error);
                }
                self.frames = None;
                self.child = Some(child);
                self.adopted = false;
                return Ok(pid);
            }

            if let Err(error) = self.stdio.logs(
                self.id.clone(),
                stdout,
                Some(stderr),
                log.clone(),
                &self.limits,
            ) {
                return abandon(&mut child, error.to_string());
            }
            self.frames = None;
        }

        self.child = Some(child);
        self.adopted = false;
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
        if let Err(error) = self.write_json_frame(&line, ipc::CONTROL_FRAME_MAX) {
            log::svcd(&format!(
                "svcd: {}: ready ping {} write failed: {}",
                self.id, id, error
            ));
        }
    }

    /// One line of readiness-handshake state for the start-timeout log.
    ///
    /// Read-only: the start wait calls it just before reporting a timeout, so a
    /// failure names which hop stalled instead of only saying "timed out".
    pub(crate) fn handshake_snapshot(&self) -> String {
        format!(
            "state={:?} pid={:?} ping_id={} stdin={} frames={} start_deadline={:?}",
            self.machine.state,
            self.machine.pid,
            self.ping_id,
            if self.stdin.is_some() {
                "open"
            } else {
                "closed"
            },
            if self.frames.is_some() {
                "attached"
            } else {
                "none"
            },
            self.start_deadline,
        )
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
                    self.note_frame_during_start("not a JSON object", line);
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
                self.note_frame_during_start("no method or response id", line);
                self.log_service(line);
            }
            return;
        };
        if self.machine.state == State::Starting {
            if id == self.ping_id {
                match self.machine.pid {
                    Some(pid) => {
                        log::svcd(&format!(
                            "svcd: {}: ready ping {} answered by pid {}",
                            self.id, id, pid
                        ));
                        self.apply(Event::Ready { pid }, now);
                    }
                    None => log::svcd(&format!(
                        "svcd: {}: ready ping {} answered before the pid was known; dropped",
                        self.id, id
                    )),
                }
                return;
            }
            // Not the ready reply; the pending-call path below keeps its own log.
            log::svcd(&format!(
                "svcd: {}: frame id {} during Starting is not the ready ping {}",
                self.id, id, self.ping_id
            ));
        }
        self.complete_pending(id, &message);
    }

    fn handle_notification(&mut self, method: &str, params: Option<&Value>) {
        if method == "notifications/progress" {
            if let Some(token) = params
                .and_then(|params| params.get("progressToken"))
                .and_then(Value::as_str)
            {
                let line = json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params.cloned().unwrap_or(Value::Null),
                })
                .to_string();
                for pending in self
                    .pending
                    .values()
                    .filter(|pending| pending.progress_token.as_deref() == Some(token))
                {
                    if let Some(client) = &pending.client {
                        client.send(line.clone());
                    }
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
        if let Some(notify) = &self.notify_sink {
            notify(method, &params.cloned().unwrap_or(Value::Null));
        }
    }

    fn complete_pending(&mut self, id: u64, message: &serde_json::Map<String, Value>) {
        let Some(pending) = self.pending.remove(&id) else {
            log::svcd(&format!(
                "svcd: {}: ignoring response for unknown request {}",
                self.id, id
            ));
            return;
        };
        let outcome = if let Some(error) = message.get("error") {
            Err(Failure::from_plugin_error(error))
        } else if let Some(result) = message.get("result") {
            let mut result = result.clone();
            match crate::platform::storage::materialize_result(&self.paths, &mut result) {
                Ok(()) => Ok(result),
                Err(error) => Err(Failure::new(
                    Class::ImplementationError,
                    Retry::Never,
                    error.detail,
                )),
            }
        } else {
            Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                "plugin returned neither result nor error",
            ))
        };
        self.mark_degraded(&outcome);
        self.finish_pending(pending, outcome);
    }

    fn finish_pending(&self, pending: PendingCall, outcome: Result<Value, Failure>) {
        self.finish_sink(pending.sink, outcome);
    }

    /// One completion point: the slot turns the call outcome into a [`Reply`]
    /// and hands it to the sink the caller supplied. It does not encode
    /// anything — the face that owns the protocol supplied the sink.
    fn finish_sink(&self, sink: ReplySink, outcome: Result<Value, Failure>) {
        sink(outcome.map(Reply::Value));
    }

    fn mark_degraded(&mut self, outcome: &Result<Value, Failure>) {
        if let Err(failure) = outcome {
            if matches!(
                failure.class,
                Class::ImplementationError | Class::Timeout | Class::WriteFailed
            ) {
                self.machine.mark_degraded(failure.detail.clone());
            }
        }
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
            let failure = Failure::new(
                Class::Timeout,
                Retry::After(std::time::Duration::from_secs(1)),
                "tools/call timed out",
            );
            let outcome = Err(failure);
            self.mark_degraded(&outcome);
            self.finish_pending(pending, outcome);
        }
    }

    fn fail_pending(&mut self, reason: &str) {
        let pending = std::mem::take(&mut self.pending);
        for (_, call) in pending {
            let failure = Failure::new(
                Class::NotRunning,
                Retry::After(std::time::Duration::from_secs(1)),
                reason,
            );
            self.machine.mark_degraded(reason.to_string());
            self.finish_pending(call, Err(failure));
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
            self.signal_force("stdout closed");
        }
    }

    fn protocol_violation(&mut self, reason: &str) {
        log::svcd(&format!(
            "svcd: {}: protocol violation: {}; killing process",
            self.id, reason
        ));
        self.log_service(&format!("protocol violation: {}", reason));
        self.fail_pending("plugin protocol violation");
        self.signal_force("protocol violation");
    }

    /// Readiness window only: note a stdout line that cannot become Ready.
    ///
    /// Bounded to `Starting` on purpose: the steady-state call path must stay
    /// silent, and this is the only window where a stray line blocks a start.
    fn note_frame_during_start(&self, reason: &str, line: &str) {
        if self.machine.state != State::Starting {
            return;
        }
        let excerpt: String = line.chars().take(200).collect();
        log::svcd(&format!(
            "svcd: {}: stdout during Starting is not usable for ready ({}): {}",
            self.id, reason, excerpt
        ));
    }

    fn log_service(&self, line: &str) {
        if let Some(sink) = &self.log {
            if let Ok(mut sink) = sink.lock() {
                let _ = sink.line(line);
            }
        }
    }

    fn check_exit(&mut self, now: u64) {
        if self.adopted {
            let Some(pid) = self.machine.pid else {
                self.adopted = false;
                return;
            };
            if self.group_alive() {
                return;
            }
            // An adopted process is not our child, so there is no exit status.
            self.note_exit(pid, None, "adopted, group gone");
            self.clear_process();
            self.apply(Event::Exited { pid, code: None }, now);
            return;
        }
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
                self.note_exit(pid, status.code(), "try_wait");
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
            self.note_exit(pid, code, "pending exit");
            self.clear_process();
            self.apply(Event::Exited { pid, code }, now);
        }
    }

    /// One line per settled exit: the code decides restart vs stop, so the
    /// daemon log must carry it instead of only the resulting state.
    fn note_exit(&self, pid: u32, code: Option<i32>, how: &str) {
        log::svcd(&format!(
            "svcd: {}: exit pid {} code {:?} while {:?} ({})",
            self.id, pid, code, self.machine.state, how
        ));
    }

    /// True while any process of the service (not just the leader) is alive.
    fn group_alive(&self) -> bool {
        let Some(pid) = self.machine.pid else {
            return false;
        };
        crate::platform::host::process::tree_alive(self.tree.as_ref(), pid)
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
                    // Wait for the pre-retry stop hook so the next attempt
                    // starts from a clean slate.
                    if self.stop_hook.is_some() {
                        self.backoff_pending = true;
                    } else {
                        self.apply(Event::BackoffElapsed, now);
                    }
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
        let command = hook_command(&self.id, "stop", hook, &self.paths);
        match stdio::spawn(command, Plan::stop_hook()) {
            Ok((child, streams)) => {
                let _ = crate::platform::host::process::adopt(&child);
                if let Some(tree) = &self.tree {
                    // The stop action's own children belong to the service tree.
                    tree.assign(&child);
                }
                let log = LogSink::open(&self.paths.logs_dir, &self.id, &self.limits)
                    .ok()
                    .map(|sink| Arc::new(Mutex::new(sink)));
                if let Some(stderr) = streams.stderr {
                    let log = log.clone();
                    if let Err(error) = stdio::log_lines(
                        format!("svcd-{}-stop-stderr", self.id),
                        stderr,
                        log,
                        &self.limits,
                    ) {
                        log::svcd(&format!(
                            "svcd: {}: stop hook stderr reader: {}",
                            self.id, error
                        ));
                    }
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
        // A stop preempted the pending restart: finish the stop, not the spawn.
        if self.machine.state == State::Stopping {
            self.cleanup_then_spawn = false;
            // A finished one-shot has no process to signal: the hook finishing
            // is the whole stop.
            if self.machine.pid.is_none() {
                self.apply(Event::StopSettled, now);
                return;
            }
            // Windows has no polite signal for an exec-ready service. Once the
            // stop hook (the only graceful path) is done, terminate the tree
            // instead of burning the whole stop window and force-killing anyway.
            #[cfg(windows)]
            if self.stdin.is_none() {
                self.signal_force("stop hook finished and stdin already closed");
                return;
            }
            self.signal_polite();
            return;
        }
        // A stable crash was cleaning up before its replacement.
        if self.cleanup_then_spawn {
            self.cleanup_then_spawn = false;
            self.apply(Event::RestartReady, now);
            return;
        }
        // A retry was waiting for this cleanup: start the next attempt now.
        if self.machine.state == State::Backoff && self.backoff_pending {
            self.backoff_pending = false;
            self.apply(Event::BackoffElapsed, now);
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
        match self.write_json_frame(&line, ipc::CONTROL_FRAME_MAX) {
            Ok(()) => log::svcd(&format!(
                "svcd: {}: polite stop: shutdown frame written, stdin closed",
                self.id
            )),
            Err(error) => log::svcd(&format!(
                "svcd: {}: polite stop: shutdown frame failed ({}), stdin closed",
                self.id, error
            )),
        }
        self.stdin = None;
        #[cfg(unix)]
        {
            if let Some(pid) = self.machine.pid {
                // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
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
            self.signal_force("no stop hook and stdin already closed");
            return;
        }
        self.signal_polite();
    }

    pub(crate) fn signal_force(&mut self, reason: &str) {
        log::svcd(&format!(
            "svcd: {}: force stop ({}): state {:?}, pid {:?}, stdin {}, child {}",
            self.id,
            reason,
            self.machine.state,
            self.machine.pid,
            if self.stdin.is_some() {
                "open"
            } else {
                "closed"
            },
            if self.child.is_some() { "held" } else { "none" },
        ));
        self.abort_stop_hook();
        // The leader may already be gone; the tree guard still owns the rest
        // (on Unix that is the process group handled below).
        if let Some(tree) = &self.tree {
            tree.terminate();
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
        #[cfg(unix)]
        {
            if let Some(pid) = self.machine.pid {
                // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
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
        self.signal_force("give up");
        self.clear_process();
    }

    fn clear_process(&mut self) {
        log::svcd(&format!(
            "svcd: {}: clear process (state {:?}, pid {:?})",
            self.id, self.machine.state, self.machine.pid
        ));
        self.fail_pending("service exited");
        self.stdio.unregister(&self.id);
        if let Some(hub) = &self.raw_hub {
            hub.unregister(&self.id);
        }
        self.child = None;
        self.adopted = false;
        self.cleanup_then_spawn = false;
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
            command_for_interpreter(interpreter, &exec.path, &exec.args, &exec.env, false)
        }
        None => command_for_parts(&exec.path, &exec.args, &exec.env, false),
    }
}

pub(crate) fn hook_command(id: &str, action: &str, hook: &Hook, paths: &Paths) -> Command {
    build_action_command(id, action, hook, true, paths)
}

/// A `lifecycle.start` action *is* the supervised foreground process, so it is
/// adoptable after a daemon restart just like `exec`.
pub(crate) fn service_hook_command(id: &str, action: &str, hook: &Hook, paths: &Paths) -> Command {
    build_action_command(id, action, hook, false, paths)
}

fn build_action_command(
    id: &str,
    action: &str,
    hook: &Hook,
    hook_lifetime: bool,
    paths: &Paths,
) -> Command {
    let mut command = match &hook.interpreter {
        Some(interpreter) => command_for_interpreter(
            interpreter,
            &hook.path,
            &hook.args,
            &hook.env,
            hook_lifetime,
        ),
        None => command_for_parts(&hook.path, &hook.args, &hook.env, hook_lifetime),
    };
    command
        .env("SVCD_SERVICE_ID", id)
        .env("SVCD_ACTION", action)
        .env("SVCD_DATA_DIR", &paths.data_dir)
        .env("SVCD_CONFIG_DIR", &paths.config_dir)
        .env("SVCD_LOG_DIR", &paths.logs_dir);
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
    hook: bool,
) -> Command {
    let mut command = Command::new(&interpreter.path);
    command
        .args(&interpreter.args)
        .arg(script)
        .args(args)
        .envs(env);
    configure(command, hook)
}

fn command_for_parts(
    path: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    hook: bool,
) -> Command {
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
            return configure(command, hook);
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
            return configure(command, hook);
        }
    }
    #[cfg(windows)]
    if extension == "cmd" || extension == "bat" {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(path).args(args).envs(env);
        return configure(command, hook);
    }
    let mut command = Command::new(path);
    command.args(args).envs(env);
    configure(command, hook)
}

/// Services are adoptable after a daemon restart, hooks are not.
fn configure(mut command: Command, hook: bool) -> Command {
    if hook {
        crate::platform::host::process::configure(
            &mut command,
            crate::platform::host::process::Kind::Hook,
        );
    } else {
        crate::platform::host::process::configure(
            &mut command,
            crate::platform::host::process::Kind::Service,
        );
    }
    command
}

/// Kill the child a failed stdio registration left behind, then surface the
/// reason. Shared by every registration failure in [`Slot::spawn`].
fn abandon(child: &mut Child, reason: impl Into<String>) -> Result<u32, String> {
    let _ = child.kill();
    let _ = child.wait();
    Err(reason.into())
}

/// Test-only sink factory: it encodes a completed call exactly the way the old
/// injected projector did, so the slot tests can still assert on the JSON line
/// while the slot itself holds no protocol knowledge.
#[cfg(test)]
fn test_reply_sink(
    reply: crate::wire::control::channel::Reply,
    external_id: Option<Value>,
) -> ReplySink {
    Box::new(move |outcome| {
        let line = match outcome {
            Ok(Reply::Value(value)) => rpc::to_line(&rpc::ok(external_id, value)),
            Ok(Reply::Stream(_)) => rpc::to_line(&rpc::err(
                external_id,
                -32000,
                "stream replies are not wired through the test slot",
            )),
            Err(failure) => json!({
                "jsonrpc": "2.0",
                "id": external_id,
                "result": {
                    "content": [{ "type": "text", "text": failure.detail }],
                    "isError": true,
                },
            })
            .to_string(),
        };
        reply.send(line);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    /// Every slot test owns its tree; nothing here reads or writes the
    /// process environment.
    fn test_paths() -> Arc<Paths> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-slot-unit-{}-{}",
            std::process::id(),
            nanos
        ));
        let paths = Paths::under(&root);
        std::fs::create_dir_all(&paths.logs_dir).expect("logs");
        Arc::new(paths)
    }

    fn test_limits() -> Arc<crate::platform::limits::Limits> {
        crate::platform::test_support::limits()
    }

    fn slot() -> Slot {
        Slot::new(
            "demo".into(),
            exec(),
            None,
            Restart::OnFailure,
            test_paths(),
            test_limits(),
        )
    }

    #[test]
    fn notify_and_raw_transport_services_are_channel_peers() {
        // #183: a channel peer must stay alive for callers; a clean exit is
        // death, not completion. Protocol 1/2 exec services own no channel.
        let mut slot = slot();
        assert!(
            slot.is_channel_peer(),
            "a notify service must stay reachable"
        );

        let mut fire_and_forget = exec();
        fire_and_forget.ready = Ready::Exec;
        slot.set_exec(fire_and_forget);
        assert!(
            !slot.is_channel_peer(),
            "exec protocol 1 is fire-and-forget"
        );

        slot.set_protocol(3);
        assert!(
            slot.is_channel_peer(),
            "protocol 3 carries the raw transport"
        );
    }

    fn queue(slot: &mut Slot, id: u64, deadline: u64) -> Receiver<String> {
        let (tx, rx) = mpsc::channel();
        slot.pending.insert(
            id,
            PendingCall {
                capability: "echo".into(),
                deadline,
                progress_token: None,
                client: Some(crate::wire::control::channel::Reply::plain(tx.clone())),
                sink: test_reply_sink(
                    crate::wire::control::channel::Reply::plain(tx),
                    Some(json!(100 + id)),
                ),
            },
        );
        rx
    }

    #[test]
    fn the_slot_handle_reports_degraded_health() {
        let mut slot = slot();
        slot.machine.state = State::Running;
        slot.machine.mark_degraded("last call failed");
        slot.pump(0);
        assert_eq!(slot.handle().probe(), Settled::Degraded("degraded".into()));
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

        let command = hook_command("demo", "stop", &hook, &test_paths());
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
    fn notifications_are_forwarded_to_the_face_sink() {
        let (tx, rx) = mpsc::channel();
        let mut slot = Slot::new(
            "slot-ui-test".into(),
            exec(),
            None,
            Restart::OnFailure,
            test_paths(),
            test_limits(),
        );
        slot.set_notifier(Arc::new(move |method: &str, params: &Value| {
            let _ = tx.send((method.to_string(), params.clone()));
        }));
        slot.handle_notification(
            "notifications/message",
            Some(&json!({"level": "info", "data": "hello"})),
        );
        let (method, params) = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("notification");
        assert_eq!(method, "notifications/message");
        assert_eq!(params["data"], "hello");
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
            crate::wire::control::channel::Reply::plain(tx),
        );

        assert!(matches!(
            result,
            Err(CallError::Failed(failure)) if failure.class == Class::Busy
        ));
    }

    #[test]
    fn a_full_call_queue_reports_busy() {
        let slot = slot();
        let handle = slot.handle();
        for _ in 0..CALL_QUEUE {
            handle
                .submit(
                    Invocation::new("echo", json!({}), Duration::from_secs(1)),
                    None,
                    Box::new(|_: Result<Reply, Failure>| {}),
                )
                .unwrap_or_else(|_| panic!("the queue must accept a call"));
        }
        let (failure, _done) = match handle.submit(
            Invocation::new("echo", json!({}), Duration::from_secs(1)),
            None,
            Box::new(|_: Result<Reply, Failure>| {}),
        ) {
            Ok(()) => panic!("a full queue must report backpressure"),
            Err(pair) => pair,
        };
        assert_eq!(failure.class, Class::Busy);
    }

    #[test]
    fn a_timed_out_completion_sink_is_called() {
        let mut slot = slot();
        slot.machine.state = State::Running;
        let (tx, rx) = mpsc::channel();
        slot.pending.insert(
            7,
            PendingCall {
                capability: "echo".into(),
                deadline: 10,
                progress_token: None,
                client: None,
                sink: Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                }),
            },
        );

        slot.expire_pending(10);

        match rx.try_recv().expect("completion") {
            Err(failure) => assert_eq!(failure.class, Class::Timeout),
            Ok(reply) => panic!("a timed out call must fail, got {reply:?}"),
        }
    }

    #[test]
    fn dropping_a_slot_fails_a_queued_call() {
        let slot = slot();
        let handle = slot.handle();
        let (tx, rx) = mpsc::channel();
        handle
            .submit(
                Invocation::new("echo", json!({}), Duration::from_secs(1)),
                None,
                Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                }),
            )
            .unwrap_or_else(|_| panic!("the queue must accept a call"));

        drop(slot);

        match rx.try_recv().expect("completion") {
            Err(failure) => assert_eq!(failure.class, Class::NotRunning),
            Ok(reply) => panic!("a dropped slot must not report success: {reply:?}"),
        }
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
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-ipc-unit-{}-{}",
            std::process::id(),
            nanos
        ));
        let paths = Arc::new(Paths::under(&root));
        fs::create_dir_all(&paths.logs_dir).expect("logs");

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
        let mut slot = Slot::new(
            "demo".into(),
            exec,
            None,
            Restart::OnFailure,
            paths,
            test_limits(),
        );
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
            crate::wire::control::channel::Reply::plain(tx),
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
            crate::wire::control::channel::Reply::plain(tx),
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
            crate::wire::control::channel::Reply::plain(tx),
        )
        .expect("hang call");
        let (crash_tx, _crash_rx) = mpsc::channel();
        let _ = slot.request_capability(
            "crash",
            json!({}),
            Some(json!(103)),
            elapsed(&started),
            None,
            crate::wire::control::channel::Reply::plain(crash_tx),
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

    /// The conformance suite lives in the kernel, so the slot-backed adapter's
    /// conformance test lives here: a domain module may reference the kernel,
    /// the reverse edge would be a layering violation (tools/dep-lint.mjs).
    fn conforming_wire(
        respond: impl Fn(&Invocation) -> Result<Value, Failure> + Send + 'static,
    ) -> crate::kernel::contract::Instance {
        let contract =
            crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
        crate::kernel::contract::Instance::new(
            contract,
            Box::new(SlotWire::new(SlotHandle::scripted(respond), None)),
        )
        .expect("valid contract")
    }

    #[test]
    fn the_slot_wire_conforms_on_the_call_surface_and_delegates_lifecycle() {
        let contract =
            crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
        crate::kernel::conformance::assert_call_conforming(
            &contract,
            || conforming_wire(|_| Ok(json!({"ok": true}))),
            |class| {
                conforming_wire(move |_| Err(Failure::new(class, Retry::Never, "injected failure")))
            },
        );
        crate::kernel::conformance::assert_delegates_lifecycle(&contract, || {
            conforming_wire(|_| Ok(json!({})))
        });
    }

    #[test]
    fn the_slot_wire_cannot_pass_the_owned_lifecycle_suite() {
        // 钉住「为什么套件要按入口拆」：槽的生命周期属于 daemon（ADR-0015），
        // SlotWire 只能拒绝 Ensure / Quiesce，所以它永远过不了生命周期那一套。
        let contract =
            crate::kernel::conformance::sample_contract(crate::kernel::contract::Limits::default());
        let violations = crate::kernel::conformance::check(
            &contract,
            || conforming_wire(|_| Ok(json!({}))),
            |_| conforming_wire(|_| Ok(json!({}))),
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains("Ensure")),
            "the slot wire must fail the owned lifecycle suite: {violations:#?}"
        );
    }

    #[test]
    fn a_cancelled_call_never_reaches_the_slot() {
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = seen.clone();
        let mut wire = SlotWire::new(
            SlotHandle::scripted(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"ok": true}))
            }),
            None,
        );

        let invocation = Invocation::new("echo", json!({}), std::time::Duration::from_secs(1));
        invocation.cancel.cancel();
        let (tx, rx) = mpsc::channel();
        wire.call(
            invocation,
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        )
        .expect("dispatch");
        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancelled result")
        {
            Err(failure) => assert_eq!(failure.class, Class::Cancelled),
            Ok(reply) => panic!("a cancelled call must not return Ok: {reply:?}"),
        }
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "the slot must not see a call that was already cancelled"
        );
    }

    #[test]
    fn a_call_returns_before_the_slot_answers() {
        let (release_tx, release_rx) = mpsc::channel();
        let mut wire = SlotWire::new(
            SlotHandle::scripted(move |_| {
                let _ = release_rx.recv();
                Ok(json!({"ok": true}))
            }),
            None,
        );

        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        wire.call(
            Invocation::new("echo", json!({}), Duration::from_secs(1)),
            Box::new(move |outcome| {
                let _ = tx.send(outcome);
            }),
        )
        .expect("dispatch");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "submit must return without waiting for the slot"
        );
        assert!(
            rx.try_recv().is_err(),
            "the result must not be ready before the slot answers"
        );
        release_tx.send(()).expect("release the scripted slot");
        let outcome = rx.recv_timeout(Duration::from_secs(1)).expect("done");
        assert!(matches!(outcome, Ok(Reply::Value(value)) if value["ok"] == true));
    }
}
