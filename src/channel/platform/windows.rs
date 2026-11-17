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

use crate::channel::{
    ControlChannel, ControlListener, ControlStream, Endpoint, PendingCall, Reply,
};
use crate::ipc;
use crate::platform::host::acl::Acl;
use crate::rpc;
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
    acl: Arc<Acl>,
}

pub(crate) struct Stream(File);

pub(crate) struct Incoming {
    name: Vec<u16>,
    acl: Arc<Acl>,
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
    pub(crate) fn bind(config: &Path, endpoint: Endpoint) -> io::Result<Self> {
        let name = wide(&pipe_name(config, endpoint));
        if pipe_exists(&name) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "svcd control pipe is already in use",
            ));
        }
        let acl = Arc::new(endpoint_acl(config, endpoint)?);
        Ok(Listener { name, acl })
    }
}

fn endpoint_acl(config: &Path, endpoint: Endpoint) -> io::Result<Acl> {
    if !crate::platform::host::acl::is_machine_wide(config) {
        return Acl::private(false);
    }
    match endpoint {
        Endpoint::User => Acl::shared(false),
        Endpoint::Admin => Acl::admin_only(false),
    }
}

impl ControlListener for Listener {
    type Stream = Stream;

    fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
        Incoming {
            name: self.name.clone(),
            acl: Arc::clone(&self.acl),
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
        let attributes = self.acl.attributes();
        let handle = unsafe {
            CreateNamedPipeW(
                self.name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
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

    fn bind(config: &Path, endpoint: Endpoint) -> io::Result<Self::Listener> {
        Listener::bind(config, endpoint)
    }

    fn into_server(
        user: Self::Listener,
        admin: Self::Listener,
        pending: SyncSender<PendingCall>,
        wake: Arc<Wake>,
        io_stack: usize,
    ) -> io::Result<Self::Server> {
        spawn_accept(
            user,
            Endpoint::User,
            pending.clone(),
            wake.clone(),
            io_stack,
        )?;
        spawn_accept(admin, Endpoint::Admin, pending, wake, io_stack)?;
        Ok(ControlBinding)
    }

    fn connect(config: &Path, endpoint: Endpoint) -> io::Result<Self::Stream> {
        let name = wide(&pipe_name(config, endpoint));
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
}

/// One thread per control connection, exactly like the old fallback.
/// One accept thread per endpoint: the thread owns the endpoint's tier, so a
/// connection can never be mislabelled.
fn spawn_accept(
    listener: Listener,
    endpoint: Endpoint,
    pending: SyncSender<PendingCall>,
    wake: Arc<Wake>,
    io_stack: usize,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name(match endpoint {
            Endpoint::User => "svcd-channel".into(),
            Endpoint::Admin => "svcd-channel-admin".into(),
        })
        .stack_size(io_stack)
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
                    .stack_size(io_stack)
                    .spawn(move || serve_connection(stream, pending, wake, endpoint));
            }
        })
        .map(|_| ())
}

fn serve_connection(
    stream: Stream,
    tx: SyncSender<PendingCall>,
    wake: Arc<Wake>,
    endpoint: Endpoint,
) {
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
                endpoint,
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

fn pipe_name(config: &Path, endpoint: Endpoint) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.to_string_lossy().hash(&mut hasher);
    let suffix = match endpoint {
        Endpoint::User => "svcd",
        Endpoint::Admin => "svcd-admin",
    };
    format!(r"\\.\pipe\{}-{:016x}", suffix, hasher.finish())
}

fn wide(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}
