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
