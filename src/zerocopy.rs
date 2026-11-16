//! Kernel-assisted transfers where the platform provides them.
//!
//! Downloads currently send file bytes through a user buffer. `sendfile`
//! (Linux) and `TransmitFile` (Windows) let the kernel move file pages to the
//! socket instead. Callers keep the read/write path as a fallback so embedded
//! targets and unusual sockets still work.

use std::fs::File;
use std::io;
#[cfg(windows)]
use std::io::{Seek, SeekFrom};
use std::net::TcpStream;

/// Send exactly `length` bytes from `file` at `offset` to `stream`.
///
/// `Ok(true)` means the platform transferred the whole range without a user
/// buffer. `Ok(false)` means this platform/API is unavailable and the caller
/// should fall back to `read` + `write`.
pub fn send_file_range(
    file: &mut File,
    stream: &mut TcpStream,
    offset: u64,
    length: u64,
) -> io::Result<bool> {
    if length == 0 {
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    {
        linux_sendfile(file, stream, offset, length)
    }
    #[cfg(windows)]
    {
        windows_transmit_file(file, stream, offset, length)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (file, stream, offset, length);
        Ok(false)
    }
}

/// One step of a non-blocking kernel file send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))] // the Windows fallback sends blocking.
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

#[cfg(target_os = "linux")]
fn linux_sendfile(
    file: &mut File,
    stream: &mut TcpStream,
    offset: u64,
    length: u64,
) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    let mut position = offset as libc::off_t;
    let mut remaining = length;
    while remaining > 0 {
        // Linux caps one sendfile call at 0x7ffff000 bytes.
        let chunk = std::cmp::min(remaining, 0x7fff_f000) as usize;
        let sent =
            unsafe { libc::sendfile(stream.as_raw_fd(), file.as_raw_fd(), &mut position, chunk) };
        if sent < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if sent == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sendfile reached EOF before the requested range",
            ));
        }
        remaining -= sent as u64;
    }
    Ok(true)
}

#[cfg(windows)]
fn windows_transmit_file(
    file: &mut File,
    stream: &mut TcpStream,
    offset: u64,
    length: u64,
) -> io::Result<bool> {
    use std::os::windows::io::{AsRawHandle, AsRawSocket};
    use windows_sys::Win32::Networking::WinSock::TransmitFile;

    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let socket = stream.as_raw_socket() as windows_sys::Win32::Networking::WinSock::SOCKET;
    let mut position = offset;
    let mut remaining = length;
    while remaining > 0 {
        file.seek(SeekFrom::Start(position))?;
        let chunk = std::cmp::min(remaining, u32::MAX as u64) as u32;
        let sent = unsafe {
            TransmitFile(
                socket,
                handle,
                chunk,
                0,
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
            )
        };
        if sent == 0 {
            return Err(io::Error::last_os_error());
        }
        position += chunk as u64;
        remaining -= chunk as u64;
    }
    Ok(true)
}
