//! Unix side of the process seam: a process group per service (#339).

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
