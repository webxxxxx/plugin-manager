//! The one `poll(2)` boundary for the Unix loops.
//!
//! Both the plugin-stdio reactor and the HTTP/control server loop used to build
//! their own `pollfd` arrays and call libc directly. This adapter owns the
//! timeout conversion, EINTR retry and the single `unsafe` block.

#![cfg(unix)]

use std::io;

/// Wait on `fds` for at most `timeout_ms` milliseconds. `EINTR` is retried so
/// callers only see a real poll failure.
pub(crate) fn poll(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    loop {
        // SAFETY: `fds` points to a live mutable slice of `pollfd` for the
        // duration of the call; `timeout_ms` is a plain integer.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
        if ready >= 0 {
            return Ok(ready as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}
