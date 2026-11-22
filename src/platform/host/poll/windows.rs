//! `WSAPoll`: the Windows readiness backend for the server loop.
//!
//! Winsock takes a compact array and rejects invalid sockets, so the adapter
//! drops `NONE` slots and copies the results back to the caller's indices. The
//! event bits are already Winsock's own (`poll.rs` names them), so nothing is
//! translated. A wait is still readiness, not completion, which keeps the
//! connection state machine identical on both hosts.
//!
//! Winsock has no `POLLRDHUP`; a whole peer close is `POLLHUP` even when the
//! caller asked for no events (pinned by a test below). A half-close with this
//! side still writable is not a readiness event here.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::AsRawSocket;
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{WSAPoll, WSAPOLLFD};

use super::{Keyed, PollFd};

pub(crate) type Key = usize;

/// No socket in this slot. Winsock has no "ignore this entry" value, so the
/// adapter compacts these out before the call.
pub(crate) const NONE: Key = 0;

// `PollFd` is handed to `WSAPoll` as a `WSAPOLLFD` array. Keep the assertion
// next to the cast so a field change cannot silently corrupt the wait set.
const _: () = {
    assert!(std::mem::size_of::<PollFd>() == std::mem::size_of::<WSAPOLLFD>());
    assert!(std::mem::align_of::<PollFd>() == std::mem::align_of::<WSAPOLLFD>());
    assert!(std::mem::offset_of!(PollFd, key) == std::mem::offset_of!(WSAPOLLFD, fd));
    assert!(std::mem::offset_of!(PollFd, events) == std::mem::offset_of!(WSAPOLLFD, events));
    assert!(std::mem::offset_of!(PollFd, revents) == std::mem::offset_of!(WSAPOLLFD, revents));
};

pub(crate) fn poll(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<usize> {
    let mut raw: Vec<WSAPOLLFD> = Vec::with_capacity(fds.len());
    let mut indices: Vec<usize> = Vec::with_capacity(fds.len());
    for (index, fd) in fds.iter_mut().enumerate() {
        fd.revents = 0;
        if fd.key == NONE {
            continue;
        }
        raw.push(WSAPOLLFD {
            fd: fd.key,
            events: fd.events,
            revents: 0,
        });
        indices.push(index);
    }
    if raw.is_empty() {
        // `WSAPoll` rejects an empty set; a wait set with only absent sources
        // still has to honour the caller's timeout.
        if timeout_ms > 0 {
            std::thread::sleep(Duration::from_millis(timeout_ms as u64));
        }
        return Ok(0);
    }
    // SAFETY: `raw` points to a live mutable array of `WSAPOLLFD` for the
    // duration of the call; the count is that array's length.
    let ready = unsafe { WSAPoll(raw.as_mut_ptr(), raw.len() as u32, timeout_ms) };
    if ready < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut count = 0;
    for (entry, index) in raw.iter().zip(&indices) {
        fds[*index].revents = entry.revents;
        if entry.revents != 0 {
            count += 1;
        }
    }
    Ok(count)
}

impl Keyed for TcpStream {
    fn key(&self) -> Key {
        self.as_raw_socket() as Key
    }
}

impl Keyed for TcpListener {
    fn key(&self) -> Key {
        self.as_raw_socket() as Key
    }
}

#[cfg(test)]
mod tests {
    use super::super::bits::{ERROR, HUP, READ};
    use super::*;
    use std::io::Write;
    use std::time::Instant;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let address = listener.local_addr().expect("address");
        let client = TcpStream::connect(address).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        server.set_nonblocking(true).expect("nonblocking server");
        client.set_nonblocking(true).expect("nonblocking client");
        (server, client)
    }

    #[test]
    fn readability_is_reported_and_a_timeout_returns_zero() {
        let (server, mut client) = pair();
        let mut fds = [PollFd::new(server.key(), READ)];
        assert_eq!(poll(&mut fds, 0).expect("empty poll"), 0);
        assert_eq!(fds[0].revents, 0);

        client.write_all(b"x").expect("write");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if poll(&mut fds, 50).expect("poll") > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "readiness never arrived");
        }
        assert!(fds[0].revents & READ != 0, "{:?}", fds[0].revents);
    }

    /// A paused stream polls with no read interest (a handler applies
    /// backpressure) and still has to notice the peer leaving. Pins whatever
    /// Winsock reports for a graceful close so the close path cannot silently
    /// lose it.
    #[test]
    fn a_peer_close_is_visible_with_zero_interest() {
        let (server, client) = pair();
        drop(client);
        let mut fds = [PollFd::new(server.key(), 0)];
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let _ = poll(&mut fds, 50).expect("poll");
            if fds[0].revents != 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a graceful close produced no event"
            );
        }
        assert!(
            fds[0].revents & (READ | HUP | ERROR) != 0,
            "{:?}",
            fds[0].revents
        );
    }
}
