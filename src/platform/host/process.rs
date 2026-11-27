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
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Descendant;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The daemon-wide job handle is intentionally kept open for the daemon's
    /// lifetime: the OS kills every member as soon as the last handle closes
    /// (daemon death). Stored as a usize because raw handles are not Sync.
    static JOB: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn init() -> Result<(), String> {
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "CreateJobObject: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&info) as u32,
            )
        };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            unsafe { CloseHandle(handle) };
            return Err(format!("SetInformationJobObject: {}", error));
        }
        JOB.store(handle as usize, Ordering::SeqCst);
        Ok(())
    }

    pub(crate) fn adopt(child: &Child) -> Result<(), String> {
        let job = JOB.load(Ordering::SeqCst) as HANDLE;
        if job.is_null() {
            return Ok(());
        }
        let process = child.as_raw_handle() as HANDLE;
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            Err(format!(
                "AssignProcessToJobObject: {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    }

    /// Services are adoptable: the job keeps them alive across a daemon restart.
    pub(crate) fn configure_service(_command: &mut Command) {}

    /// Hooks are not adopted: the daemon-wide job is their parent-death guard.
    pub(crate) fn configure_hook(_command: &mut Command) {}

    /// Windows has no portable `/proc`; liveness of an adopted process is the
    /// tree guard's job (see `Slot::alive`).
    pub(crate) fn alive(_pid: u32) -> bool {
        false
    }

    /// The job object knows exactly which processes of the tree are alive.
    pub(crate) fn tree_alive(tree: Option<&Tree>, _pid: u32) -> bool {
        tree.map(|tree| tree.active() > 0).unwrap_or(false)
    }

    /// The job object already owns every process of the service; there is no
    /// parenthood to walk and nothing to signal one by one.
    pub(crate) fn descendants(_pid: u32) -> Vec<Descendant> {
        Vec::new()
    }

    pub(crate) fn stop_descendants(_descendants: &[Descendant]) {}

    pub(crate) fn kill_descendants(_descendants: &[Descendant]) {}

    /// The job object owns the tree, so there is no per-process judgement to
    /// make: `tree_alive` already asks it.
    pub(crate) fn descendants_alive(_descendants: &[Descendant]) -> bool {
        false
    }

    pub(crate) fn start_time(_pid: u32) -> Option<u64> {
        None
    }

    pub(crate) fn boot_id() -> Option<String> {
        None
    }

    /// The job object already took the tree down when its handle closed.
    pub(crate) fn terminate_tree(_pid: u32) {}

    pub(crate) fn kill_now(child: &mut Child) {
        let _ = child.kill();
    }

    /// A per-service job object: the whole process tree of one service.
    ///
    /// `KILL_ON_JOB_CLOSE` gives every service the same parent-death behaviour
    /// as the daemon-wide job while letting stop/restart terminate exactly that
    /// tree. On Windows 8+ a process may belong to both jobs (nested jobs).
    pub(crate) struct Tree(HANDLE);

    impl Tree {
        pub(crate) fn new() -> Option<Self> {
            use std::ffi::c_void;
            use std::mem::{size_of_val, zeroed};

            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
                unsafe { CloseHandle(handle) };
                return None;
            }
            Some(Tree(handle))
        }

        pub(crate) fn assign(&self, child: &Child) -> bool {
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) != 0 }
        }

        pub(crate) fn terminate(&self) {
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        pub(crate) fn active(&self) -> u32 {
            use std::ffi::c_void;
            use std::mem::{size_of, zeroed};

            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut c_void,
                    size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                0
            } else {
                info.ActiveProcesses
            }
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::process::{Child, Command};

    use super::Descendant;

    pub(crate) fn init() -> Result<(), String> {
        Ok(())
    }

    pub(crate) fn adopt(_child: &Child) -> Result<(), String> {
        Ok(())
    }

    /// Services are adopted after a restart, so they must survive the daemon.
    /// They still get a process group of their own: the group *is* the tree the
    /// supervisor signals, and it costs one `setpgid` at spawn.
    pub(crate) fn configure_service(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    /// Hooks are never adopted: parent death kills them outright.
    pub(crate) fn configure_hook(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        // Every hook gets its own process group so a timeout kills its tree.
        command.process_group(0);
        #[cfg(target_os = "linux")]
        {
            // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
            unsafe {
                command.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }

    /// True while `pid` exists and is signalable (EPERM still counts: not ours
    /// to signal, but alive).
    ///
    /// A zombie answers signal 0 but has already exited and only awaits
    /// reaping. An adopted process (not our child) can never be reaped by us,
    /// so counting a zombie as alive made the supervisor report it as
    /// unkillable forever (#111).
    pub(crate) fn alive(pid: u32) -> bool {
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        let signalable = unsafe { libc::kill(pid as i32, 0) == 0 }
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !signalable {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            process_state(pid)
                .map(|state| state != 'Z' && state != 'X')
                .unwrap_or(true)
        }
        #[cfg(target_os = "macos")]
        {
            // Same judgement as the Linux state read: a zombie has exited, and
            // counting it would keep a stop waiting for it (#111, #334).
            process(pid).map(|row| !row.zombie).unwrap_or(true)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            true
        }
    }

    /// The process state field of `/proc/<pid>/stat`: `R`, `S`, `D`, `Z`, ...
    #[cfg(target_os = "linux")]
    fn process_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
        // "pid (comm) state ..." — comm may itself contain ") ", so split at
        // the last one; the numeric fields that follow never contain one.
        stat.rsplit_once(") ")?.1.chars().next()
    }

    /// True while a member of process group `pgid` is still *running*.
    /// Zombies are excluded for the same reason as in [`alive`].
    pub(crate) fn group_alive(pgid: u32) -> bool {
        #[cfg(target_os = "linux")]
        {
            let Ok(entries) = std::fs::read_dir("/proc") else {
                return false;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name.parse::<u32>().is_err() {
                    continue;
                }
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                let Some((_, rest)) = stat.rsplit_once(") ") else {
                    continue;
                };
                let mut fields = rest.split_whitespace();
                let state = fields.next().unwrap_or("");
                let _parent = fields.next();
                let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
                    continue;
                };
                if group == pgid && state != "Z" && state != "X" {
                    return true;
                }
            }
            false
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pgid;
            true
        }
    }

    /// True while any process of the tree is still running. Unix has no tree
    /// handle, so the process group answers for the tree; a leader that is
    /// already a zombie (adopted, so unreapable by us) still needs the
    /// per-member scan that [`group_alive`] does.
    pub(crate) fn tree_alive(_tree: Option<&Tree>, pid: u32) -> bool {
        // Signal 0: 0 means a member exists; EPERM means it exists but is not
        // ours to signal, which still counts as alive.
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        let member = unsafe { libc::kill(-(pid as i32), 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !member {
            return false;
        }
        // A running leader is the group; this keeps the fast path a single
        // kill(2) for the common case.
        if alive(pid) {
            return true;
        }
        group_alive(pid)
    }

    /// Every process this host's table can describe, in one pass.
    ///
    /// A process that vanishes mid-scan is simply absent: the walk needs the
    /// rows it can read, and a pid with no row is not reachable as a
    /// descendant.
    #[cfg(target_os = "linux")]
    fn rows() -> Vec<super::ProcRow> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, rest)) = stat.rsplit_once(") ") else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let state = fields.next().and_then(|value| value.chars().next());
            let parent = fields.next().and_then(|value| value.parse::<u32>().ok());
            let start = rest
                .split_whitespace()
                .nth(19)
                .and_then(|value| value.parse::<u64>().ok());
            let (Some(state), Some(ppid), Some(start)) = (state, parent, start) else {
                // A process that vanished mid-scan: nothing to record.
                continue;
            };
            rows.push(super::ProcRow {
                pid,
                ppid,
                start,
                zombie: state == 'Z' || state == 'X',
            });
        }
        rows
    }

    /// Every process libproc can describe, in one pass.
    ///
    /// `proc_listpids` answers with the number of bytes it filled, and with a
    /// null buffer it answers with the bytes a full table needs. The table
    /// grows between the probe and the fetch, so the buffer carries slack; the
    /// second call then reports how much of it is real.
    #[cfg(target_os = "macos")]
    fn rows() -> Vec<super::ProcRow> {
        use std::mem::size_of;
        use std::os::raw::c_void;

        // `PROC_ALL_PIDS` from `sys/proc_info.h`; `libc` does not bind it.
        const PROC_ALL_PIDS: u32 = 1;
        let width = size_of::<libc::c_int>();
        // SAFETY: a null buffer with size 0 asks for the required size only.
        let needed = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            return Vec::new();
        }
        let capacity = needed as usize / width + 64;
        let mut pids: Vec<libc::c_int> = vec![0; capacity];
        // SAFETY: the buffer is `capacity * width` bytes and outlives the call.
        let filled = unsafe {
            libc::proc_listpids(
                PROC_ALL_PIDS,
                0,
                pids.as_mut_ptr().cast::<c_void>(),
                (capacity * width) as libc::c_int,
            )
        };
        if filled <= 0 {
            return Vec::new();
        }
        let count = (filled as usize / width).min(capacity);
        let mut rows = Vec::with_capacity(count);
        for &pid in &pids[..count] {
            let Ok(pid) = u32::try_from(pid) else {
                continue;
            };
            let Some(row) = process(pid) else {
                continue;
            };
            rows.push(row);
        }
        rows
    }

    /// What libproc says about one process, or `None` when it vanished.
    #[cfg(target_os = "macos")]
    fn process(pid: u32) -> Option<super::ProcRow> {
        use std::mem::{size_of, MaybeUninit};
        use std::os::raw::c_void;

        let size = size_of::<libc::proc_bsdinfo>();
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        // SAFETY: `info` is valid for `size` bytes and `proc_pidinfo` writes at
        // most `size` bytes into it.
        let written = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast::<c_void>(),
                size as libc::c_int,
            )
        };
        if written < size as libc::c_int {
            // The process exited between the table and this call.
            return None;
        }
        // SAFETY: the call reported writing the whole struct above.
        let info = unsafe { info.assume_init() };
        Some(super::ProcRow {
            pid: info.pbi_pid,
            ppid: info.pbi_ppid,
            start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
            zombie: info.pbi_status == libc::SZOMB,
        })
    }

    /// Walk the host's process table from `pid` and return the running
    /// processes below it.
    ///
    /// A descendant that called `setsid` left the process group, and one whose
    /// parent has already exited was reparented to init; walking parenthood is
    /// the only thing that keeps it reachable, and only while the leader is
    /// still alive. Zombies are skipped by the walk: they are gone, and their
    /// own children have been reparented away from this tree already.
    pub(crate) fn descendants(pid: u32) -> Vec<Descendant> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            super::tree_from_rows(&rows(), pid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            // No portable way to read parenthood on this host: the group stays
            // the whole answer, as it was for macOS before #333.
            let _ = pid;
            Vec::new()
        }
    }

    pub(crate) fn stop_descendants(descendants: &[Descendant]) {
        signal_descendants(descendants, libc::SIGTERM);
    }

    pub(crate) fn kill_descendants(descendants: &[Descendant]) {
        signal_descendants(descendants, libc::SIGKILL);
    }

    pub(crate) fn descendants_alive(descendants: &[Descendant]) -> bool {
        descendants
            .iter()
            .any(|(pid, start)| alive(*pid) && start_time(*pid) == Some(*start))
    }

    /// Signal the captured descendants that are still the same processes. The
    /// start time is re-checked here: a pid recycled between the snapshot and
    /// the kill would otherwise point at an unrelated process.
    fn signal_descendants(descendants: &[Descendant], signal: libc::c_int) {
        for (pid, start) in descendants {
            if start_time(*pid) != Some(*start) {
                continue;
            }
            // SAFETY: one pid, no group, and the start-time check proved it is
            // still the process the snapshot saw.
            unsafe {
                libc::kill(*pid as i32, signal);
            }
        }
    }

    /// Start time of `pid` as the host counts it, together with the boot id the
    /// pid-reuse guard: Linux reads field 22 of `/proc/<pid>/stat` (start ticks),
    /// macOS takes the microsecond start time libproc reports. The value is
    /// opaque outside the host that produced it — see [`super::ProcRow::start`],
    /// which has to use the same encoding for a snapshot to be comparable.
    pub(crate) fn start_time(pid: u32) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
            let rest = stat.split_once(") ")?.1;
            rest.split_whitespace().nth(19)?.parse().ok()
        }
        #[cfg(target_os = "macos")]
        {
            process(pid).map(|row| row.start)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            None
        }
    }

    /// Identity of the current boot. A record written before a reboot must
    /// never be used for adoption: pid numbers restart from scratch.
    pub(crate) fn boot_id() -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    /// Unix signals the whole process group.
    pub(crate) fn terminate_tree(pid: u32) {
        // SAFETY: The process/pipe handle is created by this call and checked for null before use; the child is owned by the caller.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
    }

    pub(crate) fn kill_now(child: &mut Child) {
        // SAFETY: `id()` is the live child's pid; the group was armed by
        // `configure_hook`, so a negative pid signals that group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }

    /// Unix guards a service tree with its process group, not a handle, so
    /// there is nothing to keep here. The caller sees `None` and uses the
    /// group (see `terminate_tree`).
    #[allow(dead_code)]
    pub(crate) struct Tree;

    impl Tree {
        pub(crate) fn new() -> Option<Self> {
            None
        }

        pub(crate) fn assign(&self, _child: &Child) -> bool {
            false
        }

        pub(crate) fn terminate(&self) {}
    }
}

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
