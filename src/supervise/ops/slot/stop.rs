//! The stop action (`lifecycle.stop`) of one slot: at most one run, and the
//! restart work a stop can preempt. The action runs after the supervised
//! process is gone, so the service is reported stopped only once it ends.

use super::hook::{HookEnd, HookRun};
use super::*;

/// The stop action's state. A stop can preempt a restart or a crash cleanup,
/// so the flow remembers which of those events must resume once the hook ends.
pub(super) struct StopFlow {
    hook: Option<HookRun>,
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
            started: false,
            backoff_pending: false,
            cleanup_then_spawn: false,
        }
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

    pub(super) fn install(&mut self, run: HookRun) {
        self.hook = Some(run);
    }

    /// Poll the running hook; `None` while it is still alive. A hook past its
    /// deadline is killed here so the stop window cannot outlive it.
    pub(super) fn poll(&mut self, now: u64) -> Option<HookEnd> {
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
    pub(super) fn ensure_stop_hook(&mut self, now: u64) -> bool {
        if self.stop.running() {
            return true;
        }
        if !self.stop.claim() {
            return false;
        }
        let Some(hook) = self
            .lifecycle
            .as_ref()
            .and_then(|lifecycle| lifecycle.stop.as_ref())
        else {
            return false;
        };
        let command = hook_command(
            &self.id,
            "stop",
            hook,
            &self.paths,
            self.plugin_dir.as_deref(),
        );
        match stdio::spawn(command, Plan::stop_hook()) {
            Ok((child, streams)) => {
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
                        error!("{}: stop hook stderr reader: {}", self.id, error);
                    }
                }
                info!("{}: stop hook started ({})", self.id, hook.path);
                self.stop
                    .install(HookRun::new(child, now.saturating_add(hook.timeout_ms)));
                true
            }
            Err(error) => {
                error!("{}: stop hook spawn failed: {}", self.id, error);
                false
            }
        }
    }

    pub(super) fn check_stop_hook(&mut self, now: u64) {
        let Some(outcome) = self.stop.poll(now) else {
            return;
        };
        match outcome {
            HookEnd::Exited(status) => {
                info!("{}: stop action exited {}", self.id, status)
            }
            HookEnd::TimedOut => error!("{}: stop action timed out", self.id),
        }
        // The process exited first: apply that exit now. `Stopped` means the
        // stop action finished, not merely that the leader is gone.
        if let Some((pid, code)) = self.exit_after_hook.take() {
            self.clear_process();
            self.apply(Event::Exited { pid, code }, now);
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

    pub(super) fn abort_stop_hook(&mut self) {
        if self.stop.abort() {
            warn!("{}: stop hook aborted", self.id);
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
    }

    /// No `stop` action was declared. Unix already has SIGTERM as the polite
    /// signal; Windows has no signal for an exec-ready script, so kill directly
    /// instead of burning the whole stop window waiting for nothing.
    /// Host-side termination. The stop action does not run here: it runs once
    /// the process is gone, so no plugin ever has to be the one that kills.
    pub(super) fn stop_now(&mut self) {
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
