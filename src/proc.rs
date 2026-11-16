//! Parent-death protection for supervised processes.
//!
//! Without it, an abrupt svcd death (kill -9 / TerminateProcess) leaves children
//! running unsupervised; a restarted svcd then starts a second instance because
//! it never saw the leftover exit. Windows puts every child in a job object
//! with KILL_ON_JOB_CLOSE; Linux arms PR_SET_PDEATHSIG in the child before exec.
//! macOS has no equally cheap parent-death primitive, so it keeps the previous
//! behaviour (documented in the spec).

use std::process::Command;

#[cfg(windows)]
mod imp {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// The job handle is intentionally kept open for the daemon's lifetime: the
    /// OS kills every member as soon as the last handle closes (daemon death).
    /// Stored as a usize because raw handles are not Sync.
    static JOB: AtomicUsize = AtomicUsize::new(0);

    pub fn init() -> Result<(), String> {
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

    pub fn assign(child: &Child) -> Result<(), String> {
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
}

#[cfg(not(windows))]
mod imp {
    use std::process::Child;

    pub fn init() -> Result<(), String> {
        Ok(())
    }

    pub fn assign(_child: &Child) -> Result<(), String> {
        Ok(())
    }
}

pub use imp::{assign, init};

/// Start time of `pid` in clock ticks since boot, as `/proc/<pid>/stat` field
/// 22. Together with the boot id it is the pid-reuse guard for adoption.
pub fn start_time(pid: u32) -> Option<u64> {
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

/// Identity of the current boot. A record written before a reboot must never be
/// used for adoption: pid numbers restart from scratch and only look valid.
pub fn boot_id() -> Option<String> {
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

/// The process state field of `/proc/<pid>/stat`: `R`, `S`, `D`, `Z`, ...
#[cfg(target_os = "linux")]
fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // "pid (comm) state ..." — comm may itself contain ") ", so split at the
    // last one; the numeric fields that follow never contain a parenthesis.
    stat.rsplit_once(") ")?.1.chars().next()
}

/// True while `pid` exists and is signalable (EPERM still counts: not ours to
/// signal, but alive).
///
/// A zombie answers signal 0 but has already exited and only awaits reaping.
/// An adopted process (not our child) can never be reaped by us, so counting a
/// zombie as alive made the supervisor report it as unkillable forever (#111).
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
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
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// True while a member of process group `pgid` is still *running*. Zombies are
/// excluded for the same reason as in [`alive`]; the caller decides whether the
/// extra `/proc` scan is worth it (see `Slot::group_alive`).
#[cfg(target_os = "linux")]
pub fn group_alive(pgid: u32) -> bool {
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

/// Terminate a service process group that outlived its record/definition.
/// Best effort: the caller has already validated pid + starttime. Unix signals
/// the whole group; on Windows the job object already took the tree down.
pub fn terminate_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Arm a *service* process. Unlike hooks, supervised processes are adopted
/// after a daemon restart instead of being killed with their parent: the next
/// daemon reads their runtime record, validates pid + starttime and reconnects.
pub fn configure_service(command: &mut Command) {
    let _ = command;
}

/// Arm a short-lived hook. Hooks are never adopted, so parent death kills them
/// outright (Linux) exactly like the daemon-wide job object on Windows.
pub fn configure_hook(command: &mut Command) {
    configure(command)
}

/// A per-service Windows job object: the whole process tree of one service.
///
/// `KILL_ON_JOB_CLOSE` gives every service the same parent-death behaviour as
/// the daemon-wide job while letting stop/restart terminate exactly that tree.
/// On Windows 8+ a process may belong to both jobs (nested jobs).
#[cfg(windows)]
pub struct Job(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Job {
    pub fn new() -> Option<Self> {
        use std::ffi::c_void;
        use std::mem::{size_of_val, zeroed};

        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

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
        Some(Job(handle))
    }

    pub fn assign(&self, child: &std::process::Child) -> bool {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) != 0 }
    }

    /// Kill every process still in the job.
    pub fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;

        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }

    /// Number of live processes in the job; `0` also covers a failed query.
    pub fn active(&self) -> u32 {
        use std::ffi::c_void;
        use std::mem::{size_of, zeroed};

        use windows_sys::Win32::System::JobObjects::{
            JobObjectBasicAccountingInformation, QueryInformationJobObject,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        };

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

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Arm parent-death handling on a command before it is spawned. The child gets
/// SIGTERM when svcd dies; a service that ignores stdin no longer becomes an
/// orphan.
pub fn configure(command: &mut Command) {
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
