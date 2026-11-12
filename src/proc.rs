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
