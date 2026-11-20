//! The process-wide shutdown flag and the host's stop signals.
//!
//! The event loop checks the flag on every wake, so a caller on any thread (the
//! Windows SCM control handler, for example) can stop the daemon without
//! touching daemon state.

use std::sync::atomic::{AtomicBool, Ordering};

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ask the running daemon to shut down.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

/// Whether a shutdown was requested since boot.
pub fn requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// Install the host's stop signals: SIGTERM/SIGINT on Unix, the console
/// control handler on Windows.
#[cfg(unix)]
pub fn install() {
    extern "C" fn handle(_: libc::c_int) {
        request_shutdown();
    }
    // SAFETY: The handle is owned by the daemon for its whole lifetime; the pointer is used only for this call.
    unsafe {
        libc::signal(libc::SIGTERM, handle as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t);
    }
}

#[cfg(not(any(unix, windows)))]
pub fn install() {}

#[cfg(windows)]
pub fn install() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handle(_: u32) -> i32 {
        request_shutdown();
        1
    }

    // SAFETY: The handle is owned by the daemon for its whole lifetime; the pointer is used only for this call.
    unsafe {
        SetConsoleCtrlHandler(Some(handle), 1);
    }
}
