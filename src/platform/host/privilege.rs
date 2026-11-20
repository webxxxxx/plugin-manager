//! Whether this process runs with elevated privileges.
//!
//! Moved out of `install` (#137): the platform layer answers OS questions, so
//! `paths` and the CLI no longer have to reach into a domain module for it.
//! `SVCD_TEST_UNPRIVILEGED` keeps the debug-build escape hatch the self-install
//! tests rely on.

/// Debug-build seam that pins the role so install tests are deterministic on
/// runners that happen to be root/Administrator (or not). `SVCD_TEST_UNPRIVILEGED`
/// stays as the legacy "skip the privilege check" bypass.
#[cfg(debug_assertions)]
fn test_privilege() -> Option<bool> {
    match std::env::var("SVCD_TEST_PRIVILEGE").ok().as_deref() {
        Some("privileged") => Some(true),
        Some("unprivileged") => Some(false),
        _ => None,
    }
}

#[cfg(unix)]
pub fn is_privileged() -> bool {
    #[cfg(debug_assertions)]
    {
        if let Some(privileged) = test_privilege() {
            return privileged;
        }
        if std::env::var_os("SVCD_TEST_UNPRIVILEGED").is_some() {
            return true;
        }
    }
    // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub fn is_privileged() -> bool {
    #[cfg(debug_assertions)]
    {
        if let Some(privileged) = test_privilege() {
            return privileged;
        }
        if std::env::var_os("SVCD_TEST_UNPRIVILEGED").is_some() {
            return true;
        }
    }
    use std::mem::size_of;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(any(unix, windows)))]
pub fn is_privileged() -> bool {
    #[cfg(debug_assertions)]
    if let Some(privileged) = test_privilege() {
        return privileged;
    }
    false
}
