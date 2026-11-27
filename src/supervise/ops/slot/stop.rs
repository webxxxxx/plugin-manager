//! The stop action (`lifecycle.stop`) of one slot: at most one run, and the
//! restart work a stop can preempt. The action runs after the supervised
//! process is gone, so the service is reported stopped only once it ends.

use super::action::{ActionEnd, ActionRun};
use super::*;
use crate::supervise::check::stop_reason::StopReason;

/// The stop action's state. A stop can preempt a restart or a crash cleanup,
/// so the flow remembers which of those events must resume once the hook ends.
pub(super) struct StopFlow {
    hook: Option<ActionRun>,
    /// Why this stop runs; the ended instance's cause wins over the request.
    reason: StopReason,
    /// The stop action was already attempted for this process; a spawn failure
    /// is not retried.
    started: bool,
    /// The backoff elapsed while the pre-retry stop hook was still running;
    /// spawn as soon as it finishes.
    backoff_pending: bool,
    /// A stable crash is running the stop action; spawn when it finishes.
    cleanup_then_spawn: bool,
}

impl StopFlow {
    pub(super) fn new() -> StopFlow {
        StopFlow {
            hook: None,
            reason: StopReason::Stop,
            started: false,
            backoff_pending: false,
            cleanup_then_spawn: false,
        }
    }

    pub(super) fn set_reason(&mut self, reason: StopReason) {
        self.reason = reason;
    }

    pub(super) fn reason(&self) -> StopReason {
        self.reason
    }

    pub(super) fn running(&self) -> bool {
        self.hook.is_some()
    }

    /// Claim the one stop-action attempt for this process.
    pub(super) fn claim(&mut self) -> bool {
        if self.started {
            return false;
        }
        self.started = true;
        true
    }

    pub(super) fn install(&mut self, run: ActionRun) {
        self.hook = Some(run);
    }

    /// Poll the running hook; `None` while it is still alive. A hook past its
    /// deadline is killed here so the stop window cannot outlive it.
    pub(super) fn poll(&mut self, now: u64) -> Option<ActionEnd> {
        let run = self.hook.as_mut()?;
        let outcome = run.poll(now);
        if outcome.is_some() {
            self.hook = None;
        }
        outcome
    }

    /// Kill and reap the hook; true when one was running.
    pub(super) fn abort(&mut self) -> bool {
        let Some(mut run) = self.hook.take() else {
            return false;
        };
        run.abort();
        true
    }

    /// Whether this instance already ran the stop action.
    pub(super) fn claimed(&self) -> bool {
        self.started
    }

    /// A new process starts: the stop action is attempted again, and no
    /// pre-spawn work remains.
    pub(super) fn reset(&mut self) {
        self.started = false;
        self.reason = StopReason::Stop;
        self.backoff_pending = false;
        self.cleanup_then_spawn = false;
    }

    /// The backoff elapsed while the hook was still running.
    pub(super) fn defer_backoff(&mut self) {
        self.backoff_pending = true;
    }

    /// Take the deferred backoff, if any.
    pub(super) fn take_backoff(&mut self) -> bool {
        std::mem::take(&mut self.backoff_pending)
    }

    /// A stable crash wants the replacement to start once the hook ends.
    pub(super) fn defer_cleanup(&mut self) {
        self.cleanup_then_spawn = true;
    }

    /// Take the deferred spawn, if any.
    pub(super) fn take_cleanup(&mut self) -> bool {
        std::mem::take(&mut self.cleanup_then_spawn)
    }

    pub(super) fn clear_cleanup(&mut self) {
        self.cleanup_then_spawn = false;
    }
}

impl Slot {
    /// Why the stop action runs: the ended instance's cause wins over the
    /// explicit request, so a crash inside `svc stop` still reports `crash`.
    pub(super) fn stop_reason(&self) -> StopReason {
        match self.machine.cause {
            Some(cause) => StopReason::from_cause(cause),
            None => self.stop.reason(),
        }
    }

    pub(super) fn ensure_stop_action(&mut self, now: u64) -> bool {
        if self.stop.running() {
            return true;
        }
        if !self.stop.claim() {
            return false;
        }
        let Some(hook) = self.stop_action() else {
            return false;
        };
        let reason = self.stop_reason();
        // Only a process that ended on its own has an exit code to report: on a
        // stop/shutdown the process is still alive when this action spawns, and
        // a failed start may never have produced one (its last_exit would
        // belong to an older instance).
        let last_exit = match reason {
            StopReason::Crash | StopReason::Exit => self.machine.last_exit,
            _ => None,
        };
        let mut command = hook_command(
            &self.id,
            "stop",
            hook,
            &self.paths,
            self.plugin_dir.as_deref(),
        );
        // One hook branches on this instead of one hook per state: the stop
        // action runs on every path, and this says which one (#335).
        command.env("SVCD_STOP_REASON", reason.as_str()).env(
            "SVCD_LAST_EXIT",
            last_exit
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".into()),
        );
        match stdio::spawn(command, Plan::action()) {
            Ok((mut child, streams)) => {
                let _ = crate::platform::host::process::adopt(&child);
                if let Some(tree) = &self.tree {
                    // The stop action's own children belong to the service tree.
                    tree.assign(&child);
                }
                let log = LogWriter::open(
                    &self.paths.logs_dir,
                    &format!("{}-hooks", self.id),
                    &format!("hook:{}", self.id),
                    &self.limits,
                    crate::platform::log::clock(),
                    false,
                )
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
                        // Without a reader the action's stderr pipe fills and
                        // blocks: drop the attempt instead of leaking a process
                        // nobody holds.
                        error!("{}: stop action stderr reader: {}", self.id, error);
                        let _ = abandon(&mut child, error.to_string());
                        return false;
                    }
                }
                info!("{}: stop action started ({})", self.id, hook.path);
                self.stop
                    .install(ActionRun::new(child, now.saturating_add(hook.timeout_ms)));
                true
            }
            Err(error) => {
                error!("{}: stop action spawn failed: {}", self.id, error);
                false
            }
        }
    }

    pub(super) fn check_stop_action(&mut self, now: u64) {
        let Some(outcome) = self.stop.poll(now) else {
            return;
        };
        match outcome {
            ActionEnd::Exited(status) => {
                info!("{}: stop action exited {}", self.id, status)
            }
            ActionEnd::TimedOut => error!("{}: stop action timed out", self.id),
        }
        // The process exited first: apply that exit now. `Stopped` means the
        // stop action finished, not merely that the leader is gone.
        if let Some((pid, code)) = self.exit_awaiting_action.take() {
            self.clear_process();
            self.apply(Event::Exited { pid, code }, now);
            return;
        }
        // A stop arrived while the crash cleanup was still running: now that
        // it is over, the stop can settle (the action already ran).
        if std::mem::take(&mut self.stop_after_action) {
            self.apply(Event::Stop, now);
            if self.machine.state == State::Stopping && self.machine.pid.is_none() {
                self.ensure_stop_action(now);
                if !self.stop.running() {
                    self.apply(Event::StopSettled, now);
                }
            }
            return;
        }
        // A stop preempted the pending restart: finish the stop, not the spawn.
        if self.machine.state == State::Stopping {
            self.stop.clear_cleanup();
            // A finished one-shot has no process to signal: the action running
            // is the whole stop.
            if self.machine.pid.is_none() {
                self.apply(Event::StopSettled, now);
                return;
            }
            // Safety net for an old definition whose stop action terminated the
            // service itself: the action is over, so terminate what is left.
            #[cfg(windows)]
            if self.stdin.is_none() {
                self.signal_force("stop action finished and stdin already closed");
                return;
            }
            self.signal_polite();
            return;
        }
        // A stable crash was cleaning up before its replacement.
        if self.stop.take_cleanup() {
            self.apply(Event::RestartReady, now);
            return;
        }
        // A retry was waiting for this cleanup: start the next attempt now.
        if self.machine.state == State::Backoff && self.stop.take_backoff() {
            self.apply(Event::BackoffElapsed, now);
        }
    }

    pub(super) fn abort_stop_action(&mut self) {
        if self.stop.abort() {
            warn!("{}: stop action aborted", self.id);
        }
    }

    pub(super) fn signal_polite(&mut self) {
        let line = rpc::notification("svcd.shutdown", json!({}));
        match self.write_json_frame(&line, ipc::CONTROL_FRAME_MAX) {
            Ok(()) => info!(
                "{}: polite stop: shutdown frame written, stdin closed",
                self.id
            ),
            Err(error) => warn!(
                "{}: polite stop: shutdown frame failed ({}), stdin closed",
                self.id, error
            ),
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
        // The group does not reach a child that called `setsid`, and the polite
        // window is where most of them leave: ask them too (#332).
        self.descendants.polite();
    }

    /// Ask the service to stop. The host terminates the process; the stop
    /// action runs after it is gone, so a stop during the start action cancels
    /// that action instead of waiting for it.
    pub(crate) fn request_stop(&mut self, now: u64, reason: StopReason) {
        self.stop.set_reason(reason);
        self.abort_prelude();
        // Before any signal goes out: what leaves the process group has to be
        // found while the leader is still its parent (#332, ADR-0031).
        self.capture_tree();
        // A stop action from the previous attempt may still be running (crash
        // or pre-retry cleanup). Finishing it is part of the stop, so hold the
        // settlement instead of letting the next start abort it.
        if self.stop.running() && self.machine.state == State::Backoff {
            self.stop_after_action = true;
            return;
        }
        self.apply(Event::Stop, now);
        // A service with no supervised process (a finished one-shot, or a
        // definition whose process was never a child) has nothing to signal:
        // the stop action is the whole stop.
        if self.machine.state == State::Stopping && self.machine.pid.is_none() {
            self.ensure_stop_action(now);
            if !self.stop.running() {
                self.apply(Event::StopSettled, now);
            }
        }
    }

    /// Apply a settled exit. A stop action still owed runs first, so the exit
    /// that reports `Stopped` arrives after the cleanup, not before it.
    pub(super) fn settle_exit(&mut self, pid: u32, code: Option<i32>, now: u64) {
        let owes_stop = self.machine.state == State::Stopping
            && self.stop_action().is_some()
            && !self.stop.claimed();
        if owes_stop {
            self.exit_awaiting_action = Some((pid, code));
            if self.ensure_stop_action(now) {
                // The process is gone; only the action's own deadline applies.
                // A kill window left armed here would fire StopTimeout and
                // abort the cleanup it is supposed to bound (#324).
                self.stop_deadline = None;
                self.kill_deadline = None;
                return;
            }
            // The action could not run; do not hold the stop hostage to it.
            self.exit_awaiting_action = None;
        }
        self.clear_process();
        self.apply(Event::Exited { pid, code }, now);
    }

    /// Host-side termination. The stop action does not run here: it runs once
    /// the process is gone, so no plugin ever has to be the one that kills.
    pub(super) fn stop_now(&mut self) {
        // A polite signal also arrives without a stop request (start timeout,
        // stdout EOF): capture before the first signal (#334).
        self.capture_tree();
        #[cfg(windows)]
        if self.stdin.is_none() {
            self.signal_force("no polite signal on this platform");
            return;
        }
        self.signal_polite();
    }

    pub(crate) fn signal_force(&mut self, reason: &str) {
        info!(
            "{}: force stop ({}): state {:?}, pid {:?}, stdin {}, child {}",
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
        );
        self.abort_stop_action();
        // A forced stop can arrive without a stop request: shutdown paths
        // (`stop_all` on a dependency-graph error or past its budget) and
        // stdout EOF both kill the tree directly, so the snapshot has to be
        // taken here too, while the leader can still show its children (#334).
        self.capture_tree();
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
        // Anything the group could not reach, and that ignored the polite
        // signal, dies here so `Stopped` cannot be reported over it (#332).
        self.descendants.kill();
    }

    pub(super) fn give_up(&mut self) {
        if self.machine.unkillable {
            error!("{}: unkillable after stop timeout", self.id);
            // Only an unkillable process still needs terminating here. A
            // failed start has no process, and forcing would abort the stop
            // action that is cleaning up after it.
            self.signal_force("give up");
        } else {
            error!(
                "{}: failed after {} consecutive failures",
                self.id, self.machine.failures
            );
        }
        self.clear_process();
    }
}
