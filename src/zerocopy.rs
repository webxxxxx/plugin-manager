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
