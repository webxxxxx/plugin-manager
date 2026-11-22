//! Windows child pipes: overlapped named pipes for the parent ends.
//!
//! The reactor waits on the parent ends with an IOCP, so those handles must be
//! opened with `FILE_FLAG_OVERLAPPED`. Anonymous pipes (`CreatePipe`) cannot be
//! overlapped, which is why the reactor path uses a named pair: the parent end
//! is the overlapped server instance and the child end is an ordinary client
//! handle that `Command::spawn` duplicates as inheritable.
//!
//! Both ends stay non-inheritable here; the span step is the only place that
//! decides inheritance (see `stdio.rs`).

use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};

const PIPE_ACCESS_INBOUND: u32 = 0x0000_0001;
const PIPE_ACCESS_OUTBOUND: u32 = 0x0000_0002;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
/// Bytes the kernel buffers on the child side of one pipe. It is the first
/// backpressure tier: a writer blocks here, before the bounded queue matters.
const PIPE_BUFFER: u32 = 64 * 1024;

fn pipe_name(tag: &str) -> Vec<u16> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("\\\\.\\pipe\\svcd-{tag}-{}-{id}", std::process::id());
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

fn failed(handle: HANDLE) -> bool {
    handle.is_null() || handle == INVALID_HANDLE_VALUE
}

/// One named pair: overlapped server end plus the client end for the child.
fn named_pair(inbound: bool) -> io::Result<(File, File)> {
    let name = pipe_name(if inbound { "in" } else { "out" });
    let open_mode = if inbound {
        PIPE_ACCESS_INBOUND
    } else {
        PIPE_ACCESS_OUTBOUND
    } | FILE_FLAG_OVERLAPPED;
    let (out_size, in_size) = if inbound {
        (0, PIPE_BUFFER)
    } else {
        (PIPE_BUFFER, 0)
    };
    // SAFETY: the name is a live NUL-terminated UTF-16 string; a null security
    // descriptor keeps the handle non-inheritable (spawn decides inheritance).
    let server = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            out_size,
            in_size,
            0,
            ptr::null(),
        )
    };
    if failed(server) {
        return Err(io::Error::last_os_error());
    }
    let access = if inbound { GENERIC_WRITE } else { GENERIC_READ };
    // SAFETY: same live name; the client end is connected before it is returned
    // so the spawn never inherits a half-connected pipe.
    let client = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            0,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if failed(client) {
        let error = io::Error::last_os_error();
        // SAFETY: both handles are owned by this call on the error path.
        unsafe {
            CloseHandle(server);
        }
        return Err(error);
    }
    // The client opened first, so the server instance is already connected;
    // `ConnectNamedPipe` reports that as ERROR_PIPE_CONNECTED.
    // SAFETY: the server handle is a live pipe instance and no overlapped is
    // needed: the operation completes immediately.
    if unsafe { ConnectNamedPipe(server, ptr::null_mut()) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
            // SAFETY: both handles are owned by this call on the error path.
            unsafe {
                CloseHandle(server);
                CloseHandle(client);
            }
            return Err(error);
        }
    }
    // SAFETY: both calls returned fresh handles that this pair now owns.
    Ok(unsafe { (File::from_raw_handle(server), File::from_raw_handle(client)) })
}

/// `(parent_read, child_write)`, both byte pipes.
pub(crate) fn parent_read_pipe() -> io::Result<(File, File)> {
    named_pair(true)
}

/// `(child_read, parent_write)`, both byte pipes.
pub(crate) fn parent_write_pipe() -> io::Result<(File, File)> {
    let (server, client) = named_pair(false)?;
    Ok((client, server))
}
