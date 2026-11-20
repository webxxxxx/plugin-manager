//! Unix anonymous pipes: close-on-exec at creation, `O_NONBLOCK` on demand.

use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};

/// A fresh anonymous pipe, both ends close-on-exec: `(read, write)`.
///
/// `pipe2` is one syscall on Linux/Android; every other Unix creates the pipe
/// and arms `FD_CLOEXEC` with `fcntl`. Close-on-exec matters: the daemon spawns
/// plugins from a process with other threads, and an inherited pipe end would
/// keep a child's stdin open forever (the parent would never see EOF).
pub(crate) fn pipe() -> io::Result<(File, File)> {
    let mut fds = [0 as libc::c_int; 2];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: The descriptor was created by this function and is not closed until after the call.
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let created = {
        // SAFETY: The descriptor was created by this function and is not closed until after the call.
        let created = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if created == 0 {
            for fd in fds {
                // SAFETY: The descriptor was created by this function and is not closed until after the call.
                if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                    let error = io::Error::last_os_error();
                    // SAFETY: The descriptor was created by this function and is not closed until after the call.
                    unsafe {
                        libc::close(fds[0]);
                        libc::close(fds[1]);
                    }
                    return Err(error);
                }
            }
        }
        created
    };
    if created != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe`/`pipe2` returned two fresh descriptors we now own.
    Ok(unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) })
}

/// Put `file` in non-blocking mode. The poll set calls this on the ends whose
/// reads/writes it drives itself.
pub(crate) fn nonblocking(file: &File) -> io::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: The descriptor was created by this function and is not closed until after the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The descriptor was created by this function and is not closed until after the call.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
