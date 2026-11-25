use std::process::Child;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

#[cfg(test)]
use crate::kernel::contract::ProgressToken;
use crate::kernel::contract::{Contract, Invocation, Reply, ReplySink, Settled};
use crate::kernel::failure::{Class, Failure, Rejection, Retry};
use crate::kernel::registry::{Exec, Lifecycle, Ready, Restart, State};
use crate::platform::host::wake::Wake;
use crate::platform::log::{self, LogWriter};
use crate::platform::paths::Paths;
use crate::supervise::model::supervisor::{self, Action, Event, Machine};
use crate::wire::contract::stdio::{self, Plan, Streams, Target};
use crate::wire::contract::RAW_PROTOCOL;
use crate::wire::message::ipc;
use crate::wire::message::rpc;

use self::action::{ActionEnd, ActionRun};
use self::calls::PendingCalls;
use self::commands::{abandon, apply_core_child_env, command_for};
use self::frames::Frames;
use self::handle::SlotRequest;
use self::stop::StopFlow;

pub(crate) use self::commands::{apply_plugin_child_env, hook_command};
pub(crate) use self::handle::{SlotHandle, SlotWire};

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
    /// Where this plugin was installed (`<root>/<id>`), from the definition;
    /// `None` for a core service. Exported as `SVCD_PLUGIN_DIR` (#312).
    plugin_dir: Option<std::path::PathBuf>,
    limits: Arc<crate::platform::limits::Limits>,
    pub(crate) machine: Machine,
    exec: Exec,
    lifecycle: Option<Lifecycle>,
    child: Option<Child>,
    stdin: Option<stdio::WriteEnd>,
    frames: Frames,
    wake: Option<Arc<Wake>>,
    /// Who reads this service's stdout/stderr: the supervisor's poll set (Unix
    /// readiness, Windows IOCP), or reader threads when no reactor was installed.
    stdio: Target,
    raw_hub: Option<Arc<crate::wire::contract::rawhub::RawHub>>,
    log: Option<Arc<Mutex<LogWriter>>>,
    pending: PendingCalls,
    start_deadline: Option<u64>,
    stop_deadline: Option<u64>,
    kill_deadline: Option<u64>,
    stop: StopFlow,
    /// The start action (`lifecycle.start`) running before the process spawns.
    /// It is a preparation step: the process only starts once it exits.
    prelude: Option<ActionRun>,
    /// The process exited while the stop action was still owed. The exit is
    /// applied once that action ends, so `Stopped` means the cleanup finished.
    exit_awaiting_action: Option<(u32, Option<i32>)>,
    /// A stop request arrived while a stop action was still running; it is
    /// applied when that action ends.
    stop_after_action: bool,
    /// The whole service tree; on Unix the process group is the tree and this
    /// stays `None` (the platform returns no handle there).
    tree: Option<crate::platform::host::process::Tree>,
    /// The leader exited but other group members are still alive; the service
    /// stays `Running` until the whole group is gone.
    exit_while_group_alive: Option<Option<i32>>,
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
            plugin_dir: None,
            paths,
            limits,
            machine: Machine::new(restart),
            exec,
            lifecycle,
            child: None,
            stdin: None,
            frames: Frames::new(),
            wake: None,
            stdio: Target::threaded(),
            raw_hub: None,
            log: None,
            pending: PendingCalls::new(),
            start_deadline: None,
            stop_deadline: None,
            kill_deadline: None,
            stop: StopFlow::new(),
            prelude: None,
            exit_awaiting_action: None,
            stop_after_action: false,
            tree: None,
            exit_while_group_alive: None,
            ping_id: 1,
            adopted: false,
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

    /// Clear this slot's service log. A running slot owns an open writer, so
    /// the truncation goes through it; a stopped slot has no handle and the
    /// files are removed directly.
    pub(crate) fn clear_logs(&mut self) -> std::io::Result<usize> {
        match self.log.as_mut() {
            Some(writer) => match writer.lock() {
                Ok(mut writer) => writer.reset(),
                Err(_) => Ok(0),
            },
            None => {
                log::remove_log_files(&self.paths.logs_dir, &self.id, self.limits.log_rotations)
            }
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

    /// The definition's start action (a preparation step), if any.
    fn start_action(&self) -> Option<&crate::kernel::registry::Hook> {
        self.lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.start.as_ref())
    }

    /// The definition's stop action (cleanup after the process is gone), if any.
    fn stop_action(&self) -> Option<&crate::kernel::registry::Hook> {
        self.lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.stop.as_ref())
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
        info!("{}: restored finished one-shot state (no pid)", self.id);
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
                info!("{}: adopted pid {}", self.id, pid);
            }
            _ => {
                self.machine.pid = None;
                self.adopted = false;
                self.machine.state = State::Stopped;
            }
        }
    }

    pub(crate) fn set_protocol(&mut self, protocol: u32) {
        self.frames.set_protocol(protocol);
    }

    /// From the definition, not the location index: both describe the same tree.
    pub(crate) fn set_plugin_dir(&mut self, plugin_dir: Option<std::path::PathBuf>) {
        self.plugin_dir = plugin_dir;
    }

    /// Whether callers depend on this service's stdio channel staying open.
    /// The rule itself lives in `check::peer` so the definition loader and the
    /// runtime cannot drift apart.
    pub(crate) fn is_channel_peer(&self) -> bool {
        crate::supervise::check::peer::is_channel_peer(self.exec.ready, self.frames.protocol())
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
        self.frames.set_protocol_only(protocol_only);
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

    pub(crate) fn pump(&mut self, now: u64) {
        self.drain_requests(now);
        self.drain_frames(now);
        self.check_prelude(now);
        self.check_exit(now);
        self.check_stop_action(now);
        self.check_timers(now);
        self.sync_health();
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
    fn run_actions(&mut self, actions: Vec<Action>, now: u64) {
        for action in actions {
            match action {
                Action::Spawn => {
                    self.abort_stop_action();
                    self.abort_prelude();
                    self.stop.reset();
                    self.fail_pending("service is restarting");
                    self.begin_start(now);
                }
                Action::ArmStart => {
                    self.start_deadline = Some(now.saturating_add(supervisor::START_TIMEOUT_MS));
                }
                Action::SignalPolite => self.stop_now(),
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
                    self.ensure_stop_action(now);
                }
                // A stable service that just crashed also gets its stop action
                // before the replacement starts.
                Action::Cleanup => {
                    if self.ensure_stop_action(now) {
                        self.stop.defer_cleanup();
                    } else {
                        self.apply(Event::RestartReady, now);
                    }
                }
                // The instance is not coming back: the stop action still owns
                // its cleanup, and `Stopped` waits for it (`check_stop_action`
                // sends StopSettled).
                Action::CleanupThenStop => {
                    if !self.ensure_stop_action(now) {
                        self.apply(Event::StopSettled, now);
                    }
                }
                Action::Publish => {}
                Action::Withdraw => self.fail_pending("service is not available"),
                Action::GiveUp => {
                    // The start action is preparation for a process that will
                    // never run; a stop action already cleaning up is allowed
                    // to finish (it is bounded by its own timeout).
                    self.abort_prelude();
                    // No process is left to settle, but the attempt may have
                    // created resources: the stop action cleans them up, like
                    // every other exit path.
                    if self.machine.pid.is_none() && !self.stop.claimed() {
                        self.ensure_stop_action(now);
                    }
                    self.give_up();
                }
            }
        }
    }

    fn spawn_process(&mut self) -> Result<u32, String> {
        self.exit_while_group_alive = None;
        if self.tree.is_none() {
            self.tree = crate::platform::host::process::Tree::new();
            if self.tree.is_none() {
                warn!(
                    "{}: process job unavailable; stop only terminates the leader",
                    self.id
                );
            }
        }
        let plugin_dir = self.plugin_dir.as_deref();
        let mut command = command_for(&self.exec);
        match plugin_dir {
            // A core service is written by the operator and keeps the old
            // environment surface (#312). The process is its start action.
            None => apply_core_child_env(&mut command, &self.id, "start", &self.paths),
            Some(_) => apply_plugin_child_env(&mut command, plugin_dir, &self.paths),
        }
        let blob_dir = self.paths.temp_dir.clone();
        let _ = std::fs::create_dir_all(crate::platform::storage::incoming_dir(&self.paths));
        command.env("SVCD_TEMP_DIR", &blob_dir);
        let plan = match self.exec.ready {
            Ready::Notify => Plan::notify(),
            Ready::Exec => Plan::exec(self.frames.protocol()),
        };
        let (mut child, streams) =
            stdio::spawn(command, plan).map_err(|error| error.to_string())?;
        if let Err(error) = crate::platform::host::process::adopt(&child) {
            warn!("{}: parent-death protection: {}", self.id, error);
        }
        if let Some(tree) = &self.tree {
            if !tree.assign(&child) {
                warn!(
                    "{}: cannot assign the process job; stop only terminates the leader",
                    self.id
                );
            }
        }
        let pid = child.id();
        info!(
            "{}: spawned pid {} (ready={:?})",
            self.id, pid, self.exec.ready
        );
        let log = match LogWriter::open(
            &self.paths.logs_dir,
            &self.id,
            &format!("plugin:{}", self.id),
            &self.limits,
            log::clock(),
            false,
        ) {
            Ok(sink) => Some(Arc::new(Mutex::new(sink))),
            Err(error) => {
                error!("{}: cannot open service log: {}", self.id, error);
                None
            }
        };
        self.log = log.clone();

        let Streams {
            stdin,
            stdout,
            stderr,
        } = streams;

        // Protocol 3 always runs over the raw transport: the hub owns stdio,
        // stream 0 carries JSON-RPC when the instance is a channel peer, and
        // streams >= 1 carry raw payloads. Below is the JSON-only path.
        if self.frames.protocol() == RAW_PROTOCOL {
            self.stdin = None;
            let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
                return abandon(&mut child, "protocol 3 needs piped stdio");
            };
            if let Err(error) = self.start_raw_transport(stdin, stdout, stderr, log.clone()) {
                return abandon(&mut child, error);
            }
            self.child = Some(child);
            self.adopted = false;
            return Ok(pid);
        }

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
                self.frames.protocol(),
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
            self.frames.attach(receiver);
            self.pending.reset();
            self.ping_id = 1;
        } else {
            self.stdin = None;
            let Some(stdout) = stdout else {
                return abandon(&mut child, "piped stdout is missing");
            };
            let Some(stderr) = stderr else {
                return abandon(&mut child, "piped stderr is missing");
            };

            if let Err(error) = self.stdio.logs(
                self.id.clone(),
                stdout,
                Some(stderr),
                log.clone(),
                &self.limits,
            ) {
                return abandon(&mut child, error.to_string());
            }
            self.frames.detach();
        }

        self.child = Some(child);
        self.adopted = false;
        Ok(pid)
    }

    fn send_ping(&mut self) {
        let id = self.pending.allocate();
        self.ping_id = id;
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "svcd.ping",
            "params": {},
        })
        .to_string();
        if let Err(error) = self.write_json_frame(&line, ipc::CONTROL_FRAME_MAX) {
            error!("{}: ready ping {} write failed: {}", self.id, id, error);
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
            self.settle_exit(pid, None, now);
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
                self.exit_while_group_alive = Some(status.code());
                return;
            }
            if let Some(pid) = pid {
                self.note_exit(pid, status.code(), "try_wait");
                self.settle_exit(pid, status.code(), now);
            }
            return;
        }
        let Some(code) = self.exit_while_group_alive else {
            return;
        };
        if self.group_alive() {
            return;
        }
        self.exit_while_group_alive = None;
        if let Some(pid) = self.machine.pid {
            self.note_exit(pid, code, "pending exit");
            self.settle_exit(pid, code, now);
        }
    }

    /// One line per settled exit: the code decides restart vs stop, so the
    /// daemon log must carry it instead of only the resulting state.
    fn note_exit(&self, pid: u32, code: Option<i32>, how: &str) {
        info!(
            "{}: exit pid {} code {:?} while {:?} ({})",
            self.id, pid, code, self.machine.state, how
        );
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
        // The start action has its own deadline; the process window opens once
        // it finishes.
        if self.machine.state == State::Starting && self.prelude.is_none() {
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
                    if self.stop.running() {
                        self.stop.defer_backoff();
                    } else {
                        self.apply(Event::BackoffElapsed, now);
                    }
                }
            }
        }
    }
    fn clear_process(&mut self) {
        debug!(
            "{}: clear process (state {:?}, pid {:?})",
            self.id, self.machine.state, self.machine.pid
        );
        self.fail_pending("service exited");
        self.stdio.unregister(&self.id);
        if let Some(hub) = &self.raw_hub {
            hub.unregister(&self.id);
        }
        self.child = None;
        self.adopted = false;
        self.stop.clear_cleanup();
        self.stdin = None;
        self.frames.detach();
        self.start_deadline = None;
        self.stop_deadline = None;
        self.kill_deadline = None;
        self.exit_while_group_alive = None;
        self.exit_awaiting_action = None;
    }
}

mod action;
mod calls;
mod commands;
mod frames;
mod handle;
mod prelude;
mod stop;

#[cfg(test)]
mod tests;
