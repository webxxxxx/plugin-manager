//! This process's own CPU time, and the platform's hard CPU cap.
//!
//! The soft governor in the HTTP layer measures against these two reads. Both
//! are best effort by design: a host that hides /proc or denies the cgroup
//! write still gets the admission gate, it just has no kernel-enforced ceiling.

/// Milliseconds of CPU time this process has used so far (user + system).
pub(crate) fn own_cpu_ms() -> Option<u64> {
    imp::own_cpu_ms()
}

/// Install a kernel-enforced ceiling of percent on cores CPUs.
///
/// Returns a short description of the mechanism on success, or why it could
/// not be installed. A failure is not fatal: the caller falls back to the
/// admission gate, which is the portable half of the same policy.
pub(crate) fn install_hard_cap(percent: u8, cores: usize) -> Result<String, String> {
    imp::install_hard_cap(percent, cores)
}

#[cfg(unix)]
mod imp {
    use std::fs;
    use std::io::Write;

    pub(super) fn own_cpu_ms() -> Option<u64> {
        let stat = fs::read_to_string("/proc/self/stat").ok()?;
        // The comm field may contain spaces and parentheses, so split after the
        // last ')': the fields that follow are fixed.
        let rest = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // After the comm group: state, ppid, ..., minflt..., utime, stime.
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        let ticks = utime.saturating_add(stime);
        // SAFETY: sysconf with a constant name has no memory effects.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let hz = if hz > 0 { hz as u64 } else { 100 };
        Some(ticks.saturating_mul(1000) / hz)
    }

    /// cgroup v2: this process's own group, then a cpu.max write. The quota is
    /// per 100 ms period across every CPU the group may use.
    pub(super) fn install_hard_cap(percent: u8, cores: usize) -> Result<String, String> {
        let quoted = percent.min(100) as u64 * cores.max(1) as u64;
        if quoted == 0 {
            return Err("a 0% cap would stop the daemon".to_string());
        }
        let membership =
            fs::read_to_string("/proc/self/cgroup").map_err(|error| error.to_string())?;
        let path = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| "no cgroup v2 membership".to_string())?;
        let period = 100_000u64;
        let quota = quoted.saturating_mul(period) / 100;
        let file = format!("/sys/fs/cgroup{}/cpu.max", path.trim_end_matches('/'));
        let previous = fs::read_to_string(&file).map_err(|error| error.to_string())?;
        if previous.trim() == "max 100000" || previous.trim() == "max" {
            // Only take over a group nobody else is already limiting, so a
            // systemd CPUQuota= stays the source of truth.
            let mut handle = fs::OpenOptions::new()
                .write(true)
                .open(&file)
                .map_err(|error| error.to_string())?;
            handle
                .write_all(format!("{} {}", quota, period).as_bytes())
                .map_err(|error| error.to_string())?;
            Ok(format!(
                "cgroup cpu.max {} {} ({}% of {} cores)",
                quota, period, percent, cores
            ))
        } else {
            Err(format!("{} already limits this group", file))
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::mem::size_of;

    use windows_sys::Win32::Foundation::{GetLastError, FILETIME, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectCpuRateControlInformation,
        SetInformationJobObject, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
        JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
        JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn filetime_ms(time: FILETIME) -> u64 {
        // FILETIME counts 100 ns units.
        (((time.dwHighDateTime as u64) << 32) | time.dwLowDateTime as u64) / 10_000
    }

    pub(super) fn own_cpu_ms() -> Option<u64> {
        let mut creation = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exit = creation;
        let mut kernel = creation;
        let mut user = creation;
        // SAFETY: every pointer is a valid stack FILETIME for the call.
        let ok = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(filetime_ms(kernel).saturating_add(filetime_ms(user)))
    }

    /// A job object with CPU rate control. The process assigns itself, so the
    /// plugins it spawns inherit the same ceiling.
    pub(super) fn install_hard_cap(percent: u8, cores: usize) -> Result<String, String> {
        let _ = cores;
        let rate = (percent.min(100) as u32).saturating_mul(100);
        if rate == 0 {
            return Err("a 0% cap would stop the daemon".to_string());
        }
        // SAFETY: null attributes and name are the documented defaults.
        let job: HANDLE = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(format!("CreateJobObject failed ({})", last_error()));
        }
        let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate: rate },
        };
        // SAFETY: info is a valid, correctly sized CPU rate control block.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &info as *const _ as *const core::ffi::c_void,
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(format!("SetInformationJobObject failed ({})", last_error()));
        }
        // SAFETY: both handles are live; the process may already belong to a
        // job, which is exactly the failure this reports.
        let ok = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
        if ok == 0 {
            return Err(format!(
                "AssignProcessToJobObject failed ({}); another job already owns this process",
                last_error()
            ));
        }
        Ok(format!("job object cpu rate {}%", percent))
    }

    fn last_error() -> u32 {
        // SAFETY: no arguments, reads the thread's last error.
        unsafe { GetLastError() }
    }
}
