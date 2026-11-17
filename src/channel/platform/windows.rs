//! Windows control channel: a byte-mode named pipe.
//!
//! The pipe is created on demand, one instance per accepted connection. A byte
//! mode pipe opened without `FILE_FLAG_OVERLAPPED` has no read timeout, so the
//! stream peeks before every read; that is what keeps the deadline and the
//! disconnect token observable.

use std::ffi::OsStr;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{self, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, WaitNamedPipeW,
};

use crate::channel::{ControlChannel, ControlListener, ControlStream, PendingCall, Reply};
use crate::ipc;
use crate::rpc;
use crate::threads;
use crate::wake::Wake;

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_BROKEN_PIPE: u32 = 109;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_NOT_CONNECTED: u32 = 233;

pub(crate) struct Platform;

pub(crate) struct Listener {
    name: Vec<u16>,
}

pub(crate) struct Stream(File);

pub(crate) struct Incoming {
    name: Vec<u16>,
}

/// The thread-per-connection fallback owns the accept loop. It is started by
/// [`Platform::into_server`] and the value itself is only a marker the
/// composition root hands to the driver that owns the HTTP loop.
pub(crate) struct ControlBinding;

/// The wake pipe has no Windows counterpart in this channel: the supervisor's
/// reply is picked up by the thread that owns the connection.
#[derive(Clone)]
pub(crate) struct Notify;

impl Notify {
    pub(crate) fn wake(&self) {}
}

impl Listener {
    pub(crate) fn bind(config: &Path) -> io::Result<Self> {
        let name = wide(&pipe_name(config));
        if pipe_exists(&name) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "svcd control pipe is already in use",
            ));
        }
        Ok(Listener { name })
    }
}

impl ControlListener for Listener {
    type Stream = Stream;

    fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        Incoming {
            name: self.name.clone(),
        }
    }
}

fn pipe_exists(name: &[u16]) -> bool {
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE {
        unsafe { CloseHandle(handle) };
        return true;
    }
    unsafe { GetLastError() == ERROR_PIPE_BUSY }
}

impl Iterator for Incoming {
    type Item = io::Result<Stream>;

    fn next(&mut self) -> Option<Self::Item> {
        let handle = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Some(Err(io::Error::last_os_error()));
        }
        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
        if connected == 0 {
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_CONNECTED {
                unsafe { CloseHandle(handle) };
                return Some(Err(io::Error::from_raw_os_error(error as i32)));
            }
        }
        let file = unsafe { File::from_raw_handle(handle as RawHandle) };
        Some(Ok(Stream(file)))
    }
}

impl ControlStream for Stream {
    /// Wait until the pipe has bytes (or the timeout expires).
    ///
    /// A byte-mode pipe opened without `FILE_FLAG_OVERLAPPED` has no read
    /// timeout: a plain `ReadFile` would block until the supervisor answers.
    /// Peeking first makes the blocking read safe: it only runs when at least
    /// one byte is already available, so the loop can check its deadline and
    /// the disconnect token between peeks.
    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut available = 0u32;
        let peeked = unsafe {
            PeekNamedPipe(
                self.0.as_raw_handle() as _,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if peeked != 0 {
            if available > 0 {
                return Ok(true);
            }
            std::thread::sleep(timeout);
            return Ok(false);
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_BROKEN_PIPE || error == ERROR_PIPE_NOT_CONNECTED {
            // Report readable so the following read observes EOF.
            return Ok(true);
        }
        Err(io::Error::from_raw_os_error(error as i32))
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl ControlChannel for Platform {
    type Stream = Stream;
    type Listener = Listener;
    type Server = ControlBinding;

    fn bind(config: &Path) -> io::Result<Self::Listener> {
        Listener::bind(config)
    }

    fn into_server(
        listener: Self::Listener,
        pending: SyncSender<PendingCall>,
        wake: Arc<Wake>,
    ) -> io::Result<Self::Server> {
        std::thread::Builder::new()
            .name("svcd-channel".into())
            .stack_size(threads::io_stack())
            .spawn(move || {
                for incoming in listener.incoming() {
                    let stream = match incoming {
                        Ok(stream) => stream,
                        Err(error) => {
                            eprintln!("svcd: channel accept failed: {}", error);
                            continue;
                        }
                    };
                    let pending = pending.clone();
                    let wake = wake.clone();
                    let _ = std::thread::Builder::new()
                        .name("svcd-channel-conn".into())
                        .stack_size(threads::io_stack())
                        .spawn(move || serve_connection(stream, pending, wake));
                }
            })
            .map(|_| ControlBinding)
    }

    fn connect(config: &Path) -> io::Result<Self::Stream> {
        let name = wide(&pipe_name(config));
        for _ in 0..50 {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                let file = unsafe { File::from_raw_handle(handle as RawHandle) };
                return Ok(Stream(file));
            }
            let error = unsafe { GetLastError() };
            if error != ERROR_PIPE_BUSY && error != ERROR_FILE_NOT_FOUND {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "svcd control pipe is not available",
        ))
    }

    fn remove_socket(_config: &Path) {
        // A named pipe disappears with its last handle; there is no filesystem
        // entry to unlink.
    }

    fn set_private(_path: &Path) -> io::Result<()> {
        // The Windows token permission seam is the C8 DACL work: the daemon can
        // run as LocalSystem and the CLI as another user, so a current-user
        // DACL would cut the operator off. Keep the platform default until the
        // group policy (SYSTEM/Administrators + svcd group) lands.
        Ok(())
    }
}

/// One thread per control connection, exactly like the old fallback.
fn serve_connection(stream: Stream, tx: SyncSender<PendingCall>, wake: Arc<Wake>) {
    let mut reader = BufReader::new(stream);
    loop {
        let line = match ipc::read_frame_limit(&mut reader, ipc::LOCAL_FRAME_MAX) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) => {
                let response = rpc::err(None, -32700, &format!("invalid frame: {}", error));
                let _ = ipc::write_frame_limit(
                    reader.get_mut(),
                    &rpc::to_line(&response),
                    ipc::LOCAL_FRAME_MAX,
                );
                return;
            }
        };
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        // The supervisor queue is bounded (`PENDING_QUEUE_LIMIT`). Windows has
        // one thread per control connection, so a full queue blocks that
        // connection's thread here: that is the fallback's backpressure, with
        // no unbounded growth. Unix answers the same condition with -32000 on
        // the server loop (see `server::control::listener`).
        if tx
            .send(PendingCall {
                line,
                reply: Reply::plain(reply_tx),
            })
            .is_err()
        {
            return;
        }
        wake.notify();
        loop {
            let Ok(response) = reply_rx.recv() else {
                return;
            };
            let final_frame = serde_json::from_str::<serde_json::Value>(&response)
                .ok()
                .and_then(|value| value.get("id").cloned())
                .is_some();
            if ipc::write_frame_limit(reader.get_mut(), &response, ipc::LOCAL_FRAME_MAX).is_err() {
                return;
            }
            if final_frame {
                break;
            }
        }
    }
}

fn pipe_name(config: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.to_string_lossy().hash(&mut hasher);
    format!(r"\\.\pipe\svcd-{:016x}", hasher.finish())
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
