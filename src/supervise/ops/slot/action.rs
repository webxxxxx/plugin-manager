//! One lifecycle action (`lifecycle.start` / `lifecycle.stop`) run to
//! completion: the child, its deadline, and how it ended.
//!
//! Neither action is the service process: `start` prepares and exits, `stop`
//! cleans up after the process is gone. Both are one-shot children with a
//! timeout, so they share this runner.

use std::process::{Child, ExitStatus};

/// A running lifecycle action.
pub(super) struct ActionRun {
    child: Child,
    deadline: u64,
}

/// How a lifecycle action ended.
pub(super) enum ActionEnd {
    Exited(ExitStatus),
    /// The action outlived the timeout the definition declared.
    TimedOut,
}

impl ActionRun {
    pub(super) fn new(child: Child, deadline: u64) -> ActionRun {
        ActionRun { child, deadline }
    }

    /// Poll the running action; `None` while it is still alive. An action past
    /// its deadline is killed together with its process group, so a hook cannot
    /// leave children behind or outlive its window.
    pub(super) fn poll(&mut self, now: u64) -> Option<ActionEnd> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(ActionEnd::Exited(status)),
            Ok(None) if now >= self.deadline => {
                crate::platform::host::process::kill_now(&mut self.child);
                let _ = self.child.wait();
                Some(ActionEnd::TimedOut)
            }
            _ => None,
        }
    }

    /// Kill and reap the action.
    pub(super) fn abort(&mut self) {
        crate::platform::host::process::kill_now(&mut self.child);
        let _ = self.child.wait();
    }
}
