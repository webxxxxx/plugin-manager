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
//! Both implementations live in this file behind `imp`: Unix uses a process
//! group and signals, Windows a job object. Without a guard an abrupt svcd
//! death (kill -9 / TerminateProcess) leaves children running unsupervised, and
//! a restarted svcd starts a second instance because it never saw the leftover
//! exit. macOS has no equally cheap parent-death primitive, so it keeps the old
//! behaviour (documented in the spec).

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
}

/// Create the daemon-wide guard, if the host has one.
pub(crate) fn init() -> Result<(), String> {
    imp::init()
}

/// Arm a command before it is spawned.
pub(crate) fn configure(command: &mut Command, kind: Kind) {
    match kind {
        Kind::Service => imp::configure_service(command),
        Kind::Hook => imp::configure_hook(command),
    }
}

/// Stop this process's standard handles from being inherited by new children.
///
/// Rust spawns Windows children with `bInheritHandles = TRUE` and only marks the
/// handles it passes inheritable; any *already* inheritable handle (the pipes a
/// caller gave this process) still flows into the child. A daemon started by a
/// helper must not keep its caller's stdout pipe open, or a reader waiting for
/// EOF blocks until the daemon dies (#263).
#[cfg(windows)]
pub(crate) fn detach_stdio() {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        let handle = unsafe { GetStdHandle(id) };
        if !handle.is_null() {
            // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn detach_stdio() {}

/// Attach a child to the daemon-wide guard (parent-death protection).
pub(crate) fn adopt(child: &Child) -> Result<(), String> {
    imp::adopt(child)
}

/// True while `pid` is this process and can still be signalled.
pub(crate) fn alive(pid: u32) -> bool {
    imp::alive(pid)
}

/// True while any process of the service tree is still running.
///
/// Unix has no tree handle: the process group plus the leader's liveness answer.
/// Windows asks the job object; if the job could not be assigned there is no
/// guard left, and the supervisor's stop path has already said so.
pub(crate) fn tree_alive(tree: Option<&Tree>, pid: u32) -> bool {
    imp::tree_alive(tree.map(|tree| &tree.imp), pid)
}

/// Start time of `pid` as this host counts it (pid-reuse guard).
///
/// The unit is the host's: Linux start ticks since boot, macOS microseconds
/// since the epoch. It is only ever compared with another value from the same
/// host, so callers must not mix the two.
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    imp::start_time(pid)
}

/// Identity of the current boot (a pre-reboot record must not be adopted).
pub(crate) fn boot_id() -> Option<String> {
    imp::boot_id()
}

/// Terminate a process tree that outlived its record or definition.
pub(crate) fn terminate_tree(pid: u32) {
    imp::terminate_tree(pid)
}

/// One process of a service tree: its pid plus the start time that tells a
/// recycled pid apart from the process a snapshot saw.
pub(crate) type Descendant = (u32, u64);

/// One row of a host's process table: what a snapshot needs about one process.
///
/// The row is the seam between "how a host enumerates its process table"
/// (Linux `/proc`, macOS libproc) and the walk below, so the walk can be tested
/// with fabricated rows on any host — including a host that cannot run the
/// enumerator it compiles.
#[cfg_attr(windows, allow(dead_code))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcRow {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    /// The host's pid-reuse guard for this process: comparable with another
    /// observation on the same host, opaque anywhere else (Linux start ticks
    /// since boot, macOS microseconds since the epoch).
    pub(crate) start: u64,
    /// A zombie has exited and only awaits reaping. It is not a process a stop
    /// has to signal or wait for, and whatever it spawned was reparented away
    /// when it exited (#111, #334).
    pub(crate) zombie: bool,
}

/// Collect every running process below `root`, each with its start value.
///
/// Walking the parent links is what finds a `setsid` child: it left the process
/// group, so the group signal cannot reach it (#332). A zombie is skipped with
/// its subtree — it has exited, and its children were reparented to init when
/// it did. `visited` keeps a malformed table (a cycle, a duplicate row) from
/// making the walk spin; a real table contains neither.
#[cfg_attr(windows, allow(dead_code))]
fn tree_from_rows(rows: &[ProcRow], root: u32) -> Vec<Descendant> {
    let mut visited = std::collections::HashSet::new();
    visited.insert(root);
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(current) = frontier.pop() {
        for row in rows {
            if row.ppid != current || row.zombie || !visited.insert(row.pid) {
                continue;
            }
            found.push((row.pid, row.start));
            frontier.push(row.pid);
        }
    }
    found
}

/// The tree a stop has to take with it, captured while its leader is alive.
///
/// The process group is not the whole tree. A child that calls `setsid` leaves
/// the group, and dropbear's per-connection child does exactly that for every
/// connection it accepts (ssh 插件仓库的 `vendor/dropbear-2022.83/svr-main.c:334`),
/// so signalling the group alone left sshd sessions and their `sftp-server`
/// running after the plugin was stopped or uninstalled (#332).
///
/// Once the leader exits those children are reparented to init, so the only
/// moment their parenthood is still readable is before it is signalled. Each
/// host that can enumerate its process table answers with [`ProcRow`]s (Linux
/// `/proc`, macOS libproc); Windows guards the tree with a job object and
/// returns none, so the caller keeps its group-only behaviour there.
pub(crate) fn descendants(pid: u32) -> Vec<Descendant> {
    imp::descendants(pid)
}

/// Ask every captured descendant to exit, alongside the group's polite signal.
pub(crate) fn stop_descendants(descendants: &[Descendant]) {
    imp::stop_descendants(descendants)
}

/// Kill every captured descendant that outlived the polite signal.
pub(crate) fn kill_descendants(descendants: &[Descendant]) {
    imp::kill_descendants(descendants)
}

/// True while any captured descendant is still the process the snapshot saw.
///
/// The judgement lives here, next to [`alive`] and [`start_time`], because it
/// needs both: a pid that exists but is a zombie has exited and only awaits
/// reaping, and counting it would keep a stop waiting for it and end in
/// "unkillable" (#111, #334).
pub(crate) fn descendants_alive(descendants: &[Descendant]) -> bool {
    imp::descendants_alive(descendants)
}

/// Kill one child immediately, including its process group on Unix.
pub(crate) fn kill_now(child: &mut Child) {
    imp::kill_now(child)
}

#[cfg(windows)]
#[path = "windows.rs"]
mod imp;

#[cfg(not(windows))]
#[path = "unix.rs"]
mod imp;

#[cfg(all(test, windows))]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::Tree;

    /// A long-enough child that the test always observes it alive first.
    fn spawn_sleeper() -> std::process::Child {
        Command::new("cmd.exe")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .spawn()
            .expect("spawn a sleeper")
    }

    fn wait_for_exit(child: &mut std::process::Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => return false,
            }
        }
    }

    /// `KILL_ON_JOB_CLOSE`: dropping the guard is what protects the tree when
    /// the daemon dies without running any cleanup.
    #[test]
    fn dropping_the_tree_guard_kills_the_child() {
        let mut child = spawn_sleeper();
        {
            let tree = Tree::new().expect("a job object");
            assert!(tree.assign(&child), "assign the child to the job");
            assert!(
                child.try_wait().expect("try_wait").is_none(),
                "the sleeper must still be running before the drop"
            );
        }
        assert!(
            wait_for_exit(&mut child),
            "closing the job handle must kill the child"
        );
    }

    /// `terminate` is the explicit stop path; it must kill the tree even while
    /// the handle is still held.
    #[test]
    fn terminating_the_tree_kills_the_child() {
        let mut child = spawn_sleeper();
        let tree = Tree::new().expect("a job object");
        assert!(tree.assign(&child), "assign the child to the job");
        tree.terminate();
        assert!(
            wait_for_exit(&mut child),
            "terminate must kill the child while the handle is held"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// A child that called `setsid` leaves the process group, and the group
    /// signal alone cannot reach it — dropbear's per-connection child does this
    /// for every connection (#332). The snapshot taken before the signal is what
    /// lets a stop take it along.
    #[test]
    fn a_setsid_child_escapes_the_group_and_the_snapshot_kills_it() {
        let mut leader = spawn_leader();
        let pid = leader.id();
        let snapshot = wait_for_descendant(pid);
        let (child, _) = snapshot[0];
        assert!(super::alive(child), "the setsid child is running");

        // What the stop path did before #332: the group, and nothing else.
        super::terminate_tree(pid);
        assert!(wait_for(&mut leader, 2_000), "the leader must exit");
        assert!(
            super::alive(child),
            "the group signal cannot reach a setsid child"
        );

        super::kill_descendants(&snapshot);
        let deadline = Instant::now() + Duration::from_secs(2);
        while super::alive(child) {
            assert!(Instant::now() < deadline, "the snapshot must kill it");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The snapshot carries the start time, so a recycled pid is skipped rather
    /// than signalled: here the recorded start time is deliberately wrong.
    #[test]
    fn a_recycled_pid_is_not_signalled() {
        let mut leader = spawn_leader();
        let snapshot = wait_for_descendant(leader.id());
        let (child, _) = snapshot[0];
        let stale: Vec<super::Descendant> = vec![(child, 0)];
        super::kill_descendants(&stale);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            super::alive(child),
            "a stale snapshot must not kill anything"
        );
        super::kill_descendants(&snapshot);
        assert!(wait_for(&mut leader, 2_000), "cleanup the leader");
    }

    /// A child that exited but was never reaped is a zombie: it still answers
    /// `kill(pid, 0)`, but nothing is running. Counting it as alive would keep
    /// a stop waiting for a process that is already gone (#111, #334).
    #[test]
    fn a_zombie_descendant_is_not_alive() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        let start = super::start_time(pid).expect("start time");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !zombie(pid) {
            assert!(Instant::now() < deadline, "the child never became a zombie");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !super::descendants_alive(&[(pid, start)]),
            "a zombie has exited and must not keep a stop waiting"
        );
        let _ = child.wait();
    }

    /// Whether `pid` is a zombie: `/proc/<pid>/stat` state `Z` (or `X`).
    fn zombie(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) else {
            return false;
        };
        let Some((_, rest)) = stat.rsplit_once(") ") else {
            return false;
        };
        matches!(rest.chars().next(), Some('Z') | Some('X'))
    }

    fn spawn_leader() -> Child {
        Command::new("sh")
            .arg("-c")
            .arg("setsid sleep 30 >/dev/null 2>&1 & wait")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the leader")
    }

    fn wait_for_descendant(pid: u32) -> Vec<super::Descendant> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let found = super::descendants(pid);
            if !found.is_empty() {
                return found;
            }
            assert!(Instant::now() < deadline, "no descendant appeared");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for(child: &mut Child, millis: u64) -> bool {
        let deadline = Instant::now() + Duration::from_millis(millis);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}

/// The tree walk and the pid-reuse judgement that rides on it.
///
/// The rows are fabricated on purpose: the walk is the part that must behave the
/// same on every host, and the hosts that can enumerate a real table (Linux,
/// macOS) cannot run their enumerator on every platform that builds this file.
#[cfg(test)]
mod tree_tests {
    use super::{descendants_alive, tree_from_rows, ProcRow};

    fn running(pid: u32, ppid: u32, start: u64) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            start,
            zombie: false,
        }
    }

    fn exited(pid: u32, ppid: u32, start: u64) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            start,
            zombie: true,
        }
    }

    /// The snapshot is the whole subtree below the leader — and the leader
    /// never appears in its own snapshot.
    #[test]
    fn the_walk_takes_every_generation_of_this_tree_only() {
        let rows = [
            running(100, 99, 0),
            running(101, 100, 1),
            running(102, 101, 2),
            running(103, 100, 3),
            running(201, 200, 4),
        ];
        let mut found = tree_from_rows(&rows, 100);
        found.sort_unstable();
        assert_eq!(found, vec![(101, 1), (102, 2), (103, 3)]);
    }

    /// A zombie has exited; its children were reparented when it did, so the
    /// walk must not descend through it and must not report it.
    #[test]
    fn a_zombie_is_skipped_with_its_subtree() {
        let rows = [
            running(101, 100, 1),
            exited(102, 101, 2),
            running(103, 102, 3),
        ];
        assert_eq!(tree_from_rows(&rows, 100), vec![(101, 1)]);
    }

    /// A cycle or a duplicate row cannot make a real table, and must not make
    /// the walk spin when one shows up anyway.
    #[test]
    fn a_malformed_table_cannot_make_the_walk_spin() {
        let rows = [
            running(101, 100, 1),
            running(100, 101, 2),
            running(101, 100, 3),
        ];
        assert_eq!(tree_from_rows(&rows, 100), vec![(101, 1)]);
    }

    /// An empty table is what a host that cannot enumerate reports; the caller
    /// keeps its group-only behaviour, not an error.
    #[test]
    fn no_rows_means_no_descendants() {
        assert!(tree_from_rows(&[], 100).is_empty());
    }

    /// The snapshot carries a start value, so a pid whose start no longer
    /// matches is not the process the snapshot saw: not alive, not signalled.
    #[test]
    fn a_snapshot_with_a_stale_start_is_not_alive() {
        let me = std::process::id();
        assert!(!descendants_alive(&[(me, 0)]));
    }
}
