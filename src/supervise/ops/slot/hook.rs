//! One lifecycle action (`lifecycle.start` / `lifecycle.stop`) run to
//! completion: the child, its deadline, and how it ended.
//!
//! Neither action is the service process: `start` prepares and exits, `stop`
//! cleans up after the process is gone. Both are one-shot children with a
//! timeout, so they share this runner.

use std::process::{Child, ExitStatus};

/// A running lifecycle action.
pub(super) struct HookRun {
    child: Child,
    deadline: u64,
}

/// How a lifecycle action ended.
pub(super) enum HookEnd {
    Exited(ExitStatus),
    /// The action outlived the timeout the definition declared.
    TimedOut,
}

impl HookRun {
    pub(super) fn new(child: Child, deadline: u64) -> HookRun {
        HookRun { child, deadline }
    }

    /// Poll the running action; `None` while it is still alive. An action past
    /// its deadline is killed together with its process group, so a hook cannot
    /// leave children behind or outlive its window.
    pub(super) fn poll(&mut self, now: u64) -> Option<HookEnd> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(HookEnd::Exited(status)),
            Ok(None) if now >= self.deadline => {
                crate::platform::host::process::kill_now(&mut self.child);
                let _ = self.child.wait();
                Some(HookEnd::TimedOut)
            }
            _ => None,
        }
    }

    /// Kill and reap the action; true when one was running.
    pub(super) fn abort(&mut self) -> bool {
        crate::platform::host::process::kill_now(&mut self.child);
        let _ = self.child.wait();
        true
    }
}
