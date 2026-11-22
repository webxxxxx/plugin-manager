//! The one readiness-wait boundary for the connection loops.
//!
//! Both the plugin-stdio reactor and the HTTP/control server loop used to build
//! their own `pollfd` arrays and call `poll(2)` directly. The seam now owns the
//! wait, the timeout and the EINTR retry for both hosts.
//!
//! The event names below are the *host's* readiness flags expressed as stable
//! names: on Unix they are the `POLL*` constants, on Windows the Winsock ones.
//! A caller writes `READ`/`WRITE`/`CLOSE`; a backend hands its array straight to
//! the host call, so the hot path has no bit translation. `PollFd` is `repr(C)`
//! with the same layout as the host's wait-a-set entry (asserted in each
//! backend). Winsock has no "ignore this slot" value, so the Windows backend
//! compacts absent slots into a scratch array each pass and copies the ready
//! bits back by index.
//!
//! Only the server loop uses this seam on both hosts. The plugin-stdio reactor
//! waits through `poll(2)` on Unix (`wire/contract/reactor/pollset.rs`) but
//! completes overlapped named-pipe I/O through an IOCP on Windows
//! (`wire/contract/reactor/iocp.rs`, ADR-0028): child pipe handles cannot enter
//! a readiness set there.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use self::unix::{poll, Key, NONE};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use self::windows::{poll, Key, NONE};

#[cfg(unix)]
mod bits {
    /// The socket is readable.
    pub(crate) const READ: i16 = libc::POLLIN;
    /// The socket can take more output.
    pub(crate) const WRITE: i16 = libc::POLLOUT;
    /// The socket reported an error.
    pub(crate) const ERROR: i16 = libc::POLLERR;
    /// The peer closed.
    pub(crate) const HUP: i16 = libc::POLLHUP;
    /// The peer half-closed while this side may still write. Only Linux has
    /// the flag; elsewhere asking for it is a no-op.
    #[cfg(target_os = "linux")]
    pub(crate) const RDHUP: i16 = libc::POLLRDHUP;
    #[cfg(not(target_os = "linux"))]
    pub(crate) const RDHUP: i16 = 0;
    /// Revents that mean the connection must be dropped.
    pub(crate) const CLOSE: i16 = ERROR | HUP | RDHUP;
}

#[cfg(windows)]
mod bits {
    /// The socket is readable (`POLLRDNORM`).
    pub(crate) const READ: i16 = 0x0100;
    /// The socket can take more output (`POLLWRNORM`).
    pub(crate) const WRITE: i16 = 0x0010;
    /// The socket reported an error (`POLLERR`).
    pub(crate) const ERROR: i16 = 0x0001;
    /// The peer closed (`POLLHUP`).
    pub(crate) const HUP: i16 = 0x0002;
    /// Winsock has no half-close flag.
    pub(crate) const RDHUP: i16 = 0;
    /// `POLLNVAL`: the socket handle is no longer valid.
    const NVAL: i16 = 0x0004;
    /// Revents that mean the connection must be dropped.
    pub(crate) const CLOSE: i16 = ERROR | HUP | NVAL;
}

pub(crate) use self::bits::{CLOSE, RDHUP, READ, WRITE};
// The reactor and the Unix extras also name error/hangup directly; Windows
// folds them into `CLOSE`, so the names are not re-exported there.
#[cfg(unix)]
pub(crate) use self::bits::{ERROR, HUP};

/// One entry in a wait set: a socket key plus the host's event bits.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PollFd {
    pub(crate) key: Key,
    pub(crate) events: i16,
    pub(crate) revents: i16,
}

impl PollFd {
    pub(crate) fn new(key: Key, events: i16) -> Self {
        Self {
            key,
            events,
            revents: 0,
        }
    }
}

/// A socket that can enter a wait set. Implemented for the socket kinds the
/// loop holds (a listener and a connection) and, on Unix, the wake pipe.
pub(crate) trait Keyed {
    fn key(&self) -> Key;
}

/// The wait-set key of one socket.
pub(crate) fn key_of(socket: &impl Keyed) -> Key {
    socket.key()
}
