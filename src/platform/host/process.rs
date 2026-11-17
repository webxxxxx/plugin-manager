//! The process seam (S6a, #141).
//!
//! Four things every host answers about a supervised process:
//!
//! - how to arm it before spawn (`configure`),
//! - how to attach it to the host's tree guard (`adopt` / `Tree`),
//! - how to kill the whole tree (`terminate_tree` / `Tree::terminate`),
//! - whether a pid is still the process we started (`alive`, `start_time`,
//!   `boot_id`; the pid-reuse guard for adoption).
//!
//! Unix uses a process group and signals; Windows uses a job object. While the
//! seam settles (#141), the platform bodies still live in `crate::proc` and
//! this module delegates to them; a follow-up moves the bodies here.

use std::process::{Child, Command};

/// Why a process is being armed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// Supervised service: adopted after a daemon restart, so it must not die
    /// with the daemon.
    Service,
    /// Short-lived hook: dies with the daemon.
    Hook,
}

/// One service's process tree.
///
/// Only hosts that can guard a tree with a handle produce one; elsewhere the
/// caller falls back to the process group. This is why `new` returns `Option`.
pub(crate) struct Tree {
    imp: imp::Tree,
}

impl Tree {
    pub(crate) fn new() -> Option<Self> {
        imp::Tree::new().map(|imp| Self { imp })
    }

    /// Attach a freshly spawned child to this tree. `false` means the guard
    /// could not be applied and the caller must degrade to killing the leader.
    pub(crate) fn assign(&self, child: &Child) -> bool {
        self.imp.assign(child)
    }

    /// Kill every process still in the tree.
    pub(crate) fn terminate(&self) {
        self.imp.terminate();
    }

    /// Live processes in the tree; `0` also covers a failed query.
    pub(crate) fn active(&self) -> u32 {
        self.imp.active()
    }
}

#[cfg(windows)]
mod imp {
    /// The Windows job object already has exactly the shape the seam needs.
    pub(crate) type Tree = crate::proc::Job;
}

#[cfg(not(windows))]
mod imp {
    use std::process::Child;

    /// Unix guards a service tree with its process group, not a handle, so
    /// there is nothing to keep here. The caller sees `None` and uses the
    /// group (see `crate::proc::terminate_group`).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) struct Tree;

    impl Tree {
        pub(crate) fn new() -> Option<Self> {
            None
        }

        pub(crate) fn assign(&self, _child: &Child) -> bool {
            false
        }

        pub(crate) fn terminate(&self) {}

        pub(crate) fn active(&self) -> u32 {
            0
        }
    }
}

/// Create the daemon-wide guard, if the host has one.
pub(crate) fn init() -> Result<(), String> {
    crate::proc::init()
}

/// Arm a command before it is spawned.
pub(crate) fn configure(command: &mut Command, kind: Kind) {
    match kind {
        Kind::Service => crate::proc::configure_service(command),
        Kind::Hook => crate::proc::configure_hook(command),
    }
}

/// Attach a child to the daemon-wide guard (parent-death protection).
pub(crate) fn adopt(child: &Child) -> Result<(), String> {
    crate::proc::assign(child)
}

/// True while `pid` is this process and can still be signalled.
pub(crate) fn alive(pid: u32) -> bool {
    crate::proc::alive(pid)
}

/// True while the process group `pgid` may still have a running member.
///
/// Linux scans `/proc`; other Unix hosts have no cheap scan and answer `true`,
/// which is what the caller assumed before this seam existed.
pub(crate) fn group_alive(pgid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::proc::group_alive(pgid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pgid;
        true
    }
}

/// Start time of `pid` in clock ticks since boot (pid-reuse guard).
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    crate::proc::start_time(pid)
}

/// Identity of the current boot (a pre-reboot record must not be adopted).
pub(crate) fn boot_id() -> Option<String> {
    crate::proc::boot_id()
}

/// Terminate a process tree that outlived its record or definition.
pub(crate) fn terminate_tree(pid: u32) {
    crate::proc::terminate_group(pid)
}
