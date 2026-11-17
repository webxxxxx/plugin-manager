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

    /// Live processes in the tree; `0` also covers a failed query.
    pub(crate) fn active(&self) -> u32 {
        self.imp.active()
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

/// Attach a child to the daemon-wide guard (parent-death protection).
pub(crate) fn adopt(child: &Child) -> Result<(), String> {
    imp::adopt(child)
}

/// True while `pid` is this process and can still be signalled.
pub(crate) fn alive(pid: u32) -> bool {
    imp::alive(pid)
}

/// True while the process group `pgid` may still have a running member.
///
/// Linux scans `/proc`; other Unix hosts have no cheap scan and answer `true`,
/// which is what the caller assumed before this seam existed. Windows has no
/// process groups here: the tree guard answers instead.
pub(crate) fn group_alive(pgid: u32) -> bool {
    imp::group_alive(pgid)
}

/// Start time of `pid` in clock ticks since boot (pid-reuse guard).
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

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::{Child, Command};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(format!(
                "CreateJobObject: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
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

    /// No process groups here; the tree guard answers instead.
    pub(crate) fn group_alive(_pgid: u32) -> bool {
        true
    }

    pub(crate) fn start_time(_pid: u32) -> Option<u64> {
        None
    }

    pub(crate) fn boot_id() -> Option<String> {
        None
    }

    /// The job object already took the tree down when its handle closed.
    pub(crate) fn terminate_tree(_pid: u32) {}

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

            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of_val(&info) as u32,
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return None;
            }
            Some(Tree(handle))
        }

        pub(crate) fn assign(&self, child: &Child) -> bool {
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) != 0 }
        }

        pub(crate) fn terminate(&self) {
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        pub(crate) fn active(&self) -> u32 {
            use std::ffi::c_void;
            use std::mem::{size_of, zeroed};

            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
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
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::process::{Child, Command};

    pub(crate) fn init() -> Result<(), String> {
        Ok(())
    }

    pub(crate) fn adopt(_child: &Child) -> Result<(), String> {
        Ok(())
    }

    /// Services are adopted after a restart, so they must survive the daemon.
    pub(crate) fn configure_service(_command: &mut Command) {}

    /// Hooks are never adopted: parent death kills them outright.
    pub(crate) fn configure_hook(command: &mut Command) {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = command;
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
        let signalable = unsafe { libc::kill(pid as i32, 0) == 0 }
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !signalable {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            return process_state(pid)
                .map(|state| state != 'Z' && state != 'X')
                .unwrap_or(true);
        }
        #[cfg(not(target_os = "linux"))]
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
                let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok())
                else {
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

    /// Start time of `pid` in clock ticks since boot, as `/proc/<pid>/stat`
    /// field 22. Together with the boot id it is the pid-reuse guard.
    pub(crate) fn start_time(pid: u32) -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
            let rest = stat.split_once(") ")?.1;
            rest.split_whitespace().nth(19)?.parse().ok()
        }
        #[cfg(not(target_os = "linux"))]
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
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
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

        pub(crate) fn active(&self) -> u32 {
            0
        }
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
