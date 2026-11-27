//! Windows side of the process seam: one job object per daemon (#339).

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
