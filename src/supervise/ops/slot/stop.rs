//! The stop action (`lifecycle.stop`) of one slot: at most one hook run, its
//! deadline, and the restart work a stop can preempt.

use std::process::{Child, ExitStatus};

/// A running stop action.
pub(super) struct HookRun {
    child: Child,
    deadline: u64,
}

impl HookRun {
    pub(super) fn new(child: Child, deadline: u64) -> HookRun {
        HookRun { child, deadline }
    }
}

/// How a stop action ended.
pub(super) enum HookEnd {
    Exited(ExitStatus),
    /// The hook outlived the timeout the definition declared.
    TimedOut,
}

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
        let deadline = run.deadline;
        let outcome = match run.child.try_wait() {
            Ok(Some(status)) => Some(HookEnd::Exited(status)),
            Ok(None) if now >= deadline => {
                let _ = run.child.kill();
                let _ = run.child.wait();
                Some(HookEnd::TimedOut)
            }
            _ => None,
        };
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
        let _ = run.child.kill();
        let _ = run.child.wait();
        true
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
