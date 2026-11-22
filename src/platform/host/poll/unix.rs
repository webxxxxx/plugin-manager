//! `poll(2)`: the wait set is the host's `pollfd` array.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

use super::{Keyed, PollFd};

pub(crate) type Key = std::os::unix::io::RawFd;

/// No socket in this slot. `poll(2)` ignores a negative fd, so the entry can
/// stay in place and keep the caller's index stable.
pub(crate) const NONE: Key = -1;

// SAFETY-relevant layout guarantee: `PollFd` is handed to `libc::poll` as a
// `pollfd` array. Keep the assertion next to the cast so a field change cannot
// silently corrupt the wait set.
const _: () = {
    assert!(std::mem::size_of::<PollFd>() == std::mem::size_of::<libc::pollfd>());
    assert!(std::mem::align_of::<PollFd>() == std::mem::align_of::<libc::pollfd>());
    assert!(std::mem::offset_of!(PollFd, key) == std::mem::offset_of!(libc::pollfd, fd));
    assert!(std::mem::offset_of!(PollFd, events) == std::mem::offset_of!(libc::pollfd, events));
    assert!(std::mem::offset_of!(PollFd, revents) == std::mem::offset_of!(libc::pollfd, revents));
};

/// Wait on `fds` for at most `timeout_ms` milliseconds. `EINTR` is retried so
/// callers only see a real poll failure.
pub(crate) fn poll(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    loop {
        // SAFETY: `PollFd` has the exact layout of `libc::pollfd` (asserted
        // above); the slice is live and mutable for the call; the count is its
        // length and `timeout_ms` is a plain integer.
        let ready = unsafe {
            libc::poll(
                fds.as_mut_ptr().cast::<libc::pollfd>(),
                fds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
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

impl Keyed for TcpStream {
    fn key(&self) -> Key {
        self.as_raw_fd()
    }
}

impl Keyed for TcpListener {
    fn key(&self) -> Key {
        self.as_raw_fd()
    }
}

impl Keyed for UnixStream {
    fn key(&self) -> Key {
        self.as_raw_fd()
    }
}
