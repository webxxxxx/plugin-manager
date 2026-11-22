//! Kernel-assisted transfers where the platform provides them.
//!
//! The Unix server loop uses `try_send_file_range` after a stream handler asks
//! for a file send. Callers keep a read/write path for platforms without an
//! incremental kernel call, so embedded targets and unusual sockets still
//! work.

use std::fs::File;
use std::io;
use std::net::TcpStream;

/// One step of a non-blocking kernel file send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))] // used by the Unix server loop.
pub enum FileSendProgress {
    /// Some bytes went out; the caller retries from the new offset.
    Sent(u64),
    /// The whole requested range is on the wire.
    Done,
    /// The socket would block; retry when it is writable.
    WouldBlock,
    /// This platform has no incremental kernel send; fall back to read+write.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Unsupported,
}

/// Send part of `file` at `offset` to a non-blocking `stream` without waiting.
///
/// The server loop uses this after a stream handler returns a file send. On a
/// platform without an incremental kernel call the caller keeps its own
/// bounded read+write fallback.
#[cfg_attr(not(unix), allow(dead_code))] // used by the Unix server loop.
pub fn try_send_file_range(
    file: &mut File,
    stream: &mut TcpStream,
    offset: u64,
    length: u64,
) -> io::Result<FileSendProgress> {
    if length == 0 {
        return Ok(FileSendProgress::Done);
    }
    #[cfg(target_os = "linux")]
    {
        linux_try_sendfile(file, stream, offset, length)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, stream, offset, length);
        Ok(FileSendProgress::Unsupported)
    }
}

#[cfg(target_os = "linux")]
fn linux_try_sendfile(
    file: &mut File,
    stream: &mut TcpStream,
    offset: u64,
    length: u64,
) -> io::Result<FileSendProgress> {
    use std::os::fd::AsRawFd;

    let mut position = offset as libc::off_t;
    // Linux caps one sendfile call at 0x7ffff000 bytes.
    let chunk = std::cmp::min(length, 0x7fff_f000) as usize;
    let sent =
        // SAFETY: The file descriptor and offset come from the live download stream; the kernel validates the range.
        unsafe { libc::sendfile(stream.as_raw_fd(), file.as_raw_fd(), &mut position, chunk) };
    if sent < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::Interrupted {
            return Ok(FileSendProgress::WouldBlock);
        }
        return Err(error);
    }
    if sent == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "sendfile reached EOF before the requested range",
        ));
    }
    let sent = sent as u64;
    if sent >= length {
        Ok(FileSendProgress::Done)
    } else {
        Ok(FileSendProgress::Sent(sent))
    }
}
