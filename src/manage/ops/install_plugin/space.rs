//! Space planning for installs: free bytes, an existing version's size, and
//! the one out-of-space failure message both strategies share.

use super::*;

pub(super) fn free_space(path: &Path) -> Result<u64, ProblemError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            Problem::Internal.error(format!("free space: invalid path {}", path.display()))
        })?;
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
            return Err(Problem::Internal.error(format!(
                "free space {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        if unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free)
        } == 0
        {
            return Err(Problem::Internal.error(format!(
                "free space {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(Problem::Internal.error("free space is not supported on this platform"))
    }
}

pub(super) fn directory_size(path: &Path) -> Result<u64, ProblemError> {
    let mut total = 0u64;
    let entries = fs::read_dir(path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            Problem::Internal.error(format!("read {}: {}", path.display(), error))
        })?;
        let metadata = entry.metadata().map_err(|error| {
            Problem::Internal.error(format!("stat {}: {}", entry.path().display(), error))
        })?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// The failure a full disk produces. One message shape for both strategies.
pub(super) fn out_of_space(package: &Package, free: u64, old_size: Option<u64>) -> ProblemError {
    Problem::Internal.error(format!(
        "not enough space to install {} {}: need {} bytes, {} free{}",
        package.manifest.id(),
        package.manifest.version,
        package.unpacked_size(),
        free,
        old_size
            .map(|size| format!(", current version uses {size} bytes"))
            .unwrap_or_default()
    ))
}
