//! Windows anonymous pipes: `CreatePipe`, and inheritance decided at spawn.

use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Pipes::CreatePipe;

/// A fresh anonymous pipe: `(read, write)`.
///
/// Both ends are created **non-inheritable** (a null security descriptor).
/// `Command::spawn` duplicates exactly the child end as inheritable under its
/// spawn lock, so the parent end never leaks into a child. That matters here:
/// the daemon spawns with inherited stdio, and a leaked parent end would keep
/// the pipe's other side open — the parent would then poll a child that has
/// already exited, or never see EOF on a plugin's stdin. Handle inheritance is
/// therefore a property of the spawn, not of the pipe: it lives in std's
/// duplication step, and both ends here stay private to this process.
pub(crate) fn pipe() -> io::Result<(File, File)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: two valid out-pointers; no security descriptor means neither end
    // is inheritable.
    if unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `CreatePipe` returned two fresh handles that we now own.
    Ok(unsafe { (File::from_raw_handle(read), File::from_raw_handle(write)) })
}
