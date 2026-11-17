use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
#[cfg(not(unix))]
use std::sync::mpsc::SyncSender;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(not(unix))]
pub use imp::Listener;
pub use imp::Stream;

use crate::ipc;
#[cfg(not(unix))]
use crate::rpc;
#[cfg(not(unix))]
use crate::threads;

/// One message from an outbound streaming management call. The control
/// channel owns this shape; the MCP session consumes it. The Unix server-loop
/// client produces it today, so Windows sees the type unused.
#[derive(Debug)]
#[cfg_attr(any(not(unix), not(feature = "http")), allow(dead_code))]
pub enum ProgressEvent {
    /// A progress notification line (a JSON-RPC message without an id).
    Frame(String),
    /// The final JSON-RPC response line.
    Final(String),
    /// The outbound stream could not be opened or read.
    Error(String),
}

/// Bound on queued management requests between the server loop and the
/// supervisor loop. A full queue is a visible error, not memory growth; each
/// control connection has at most one request in flight.
pub const PENDING_QUEUE_LIMIT: usize = 256;

pub struct PendingCall {
    pub line: String,
    pub reply: Reply,
}

/// A response handle for one management request. The daemon sends the serialized
/// response on the channel; the Unix server loop also gets a byte on a
/// notifier pipe so it can wake immediately instead of polling.
#[derive(Clone)]
pub struct Reply {
    sender: Sender<String>,
    #[cfg(unix)]
    notify: Option<std::sync::Arc<std::os::unix::net::UnixStream>>,
}

impl Reply {
    #[allow(dead_code)]
    pub fn plain(sender: Sender<String>) -> Self {
        Reply {
            sender,
            #[cfg(unix)]
            notify: None,
        }
    }

    #[cfg(unix)]
    pub fn with_notify(
        sender: Sender<String>,
        notify: std::sync::Arc<std::os::unix::net::UnixStream>,
    ) -> Self {
        Reply {
            sender,
            notify: Some(notify),
        }
    }

    pub fn send(&self, response: String) {
        if self.sender.send(response).is_ok() {
            #[cfg(unix)]
            if let Some(notify) = &self.notify {
                use std::io::Write;
                let _ = (&**notify).write(&[1]);
            }
        }
    }
}

pub fn token_path(config: &Path) -> PathBuf {
    config.join("svcd.token")
}

/// Read the local control token, creating it on first daemon start.
pub fn ensure_token(config: &Path) -> io::Result<String> {
    let path = token_path(config);
    if let Ok(existing) = fs::read_to_string(&path) {
        let token = existing.trim().to_string();
        if !token.is_empty() {
            set_private(&path)?;
            return Ok(token);
        }
    }
    let token = random_token();
    fs::write(&path, &token)?;
    set_private(&path)?;
    Ok(token)
}

pub fn read_token(config: &Path) -> io::Result<String> {
    let token = fs::read_to_string(token_path(config))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "empty token"));
    }
    Ok(token)
}

fn set_private(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The svcd group may read the token; local management is group/root only.
        fs::set_permissions(path, fs::Permissions::from_mode(0o640))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn random_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut token = String::with_capacity(32);
    for _ in 0..2 {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        );
        hasher.write_u32(std::process::id());
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

/// Bind the local control channel. Unix hands the listener to the server loop;
/// Windows keeps the thread-per-connection fallback.
#[cfg(unix)]
pub fn bind_server(config: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    let path = config.join("svcd.sock");
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "svcd control socket is already in use",
            ));
        }
        let _ = fs::remove_file(&path);
    }
    let listener = UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

#[cfg(not(unix))]
pub fn spawn_server(
    config: &Path,
    tx: SyncSender<PendingCall>,
    wake: std::sync::Arc<crate::wake::Wake>,
) -> io::Result<()> {
    let listener = Listener::bind(config)?;
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
                let tx = tx.clone();
                let wake = wake.clone();
                let _ = std::thread::Builder::new()
                    .name("svcd-channel-conn".into())
                    .stack_size(threads::io_stack())
                    .spawn(move || serve(stream, tx, wake));
            }
        })
        .map(|_| ())
}

#[cfg(not(unix))]
fn serve(stream: Stream, tx: SyncSender<PendingCall>, wake: std::sync::Arc<crate::wake::Wake>) {
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

const POOL_MAX: usize = 8;

fn pool() -> &'static Mutex<HashMap<PathBuf, Vec<Stream>>> {
    static POOL: OnceLock<Mutex<HashMap<PathBuf, Vec<Stream>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acquire(config: &Path) -> io::Result<Stream> {
    if let Ok(mut pool) = pool().lock() {
        if let Some(stream) = pool.get_mut(config).and_then(Vec::pop) {
            return Ok(stream);
        }
    }
    connect(config)
}

fn release(config: &Path, stream: Stream) {
    if let Ok(mut pool) = pool().lock() {
        let connections = pool.entry(config.to_path_buf()).or_default();
        if connections.len() < POOL_MAX {
            connections.push(stream);
        }
    }
}

pub fn connect(config: &Path) -> io::Result<Stream> {
    imp::connect(config)
}

/// One request/response round trip on the local management channel.
pub fn request(
    config: &Path,
    method: &str,
    params: serde_json::Value,
) -> io::Result<crate::rpc::Response> {
    request_stream(config, method, params, None, || false, |_| {})
}

/// A request whose handler may emit JSON-RPC notifications before the final
/// response. Notifications are delivered to `on_notification`; reading stops at
/// the response with an `id`.
///
/// `idle` bounds how long the caller waits for the next frame; a bound also
/// turns the read into a short poll so `cancelled` can be observed. The CLI
/// passes no bound and never cancels, so it keeps the old blocking read and
/// the connection pool. A server-side SSE response passes the transfer bound
/// and its client's disconnect check, so a hung supervisor or a gone client
/// releases the stream permit instead of pinning the connection thread. A
/// timed-out or cancelled socket is dropped, never returned to the pool: a
/// late response would otherwise be mistaken for the next request's reply.
pub fn request_stream<F, C>(
    config: &Path,
    method: &str,
    params: serde_json::Value,
    idle: Option<Duration>,
    cancelled: C,
    on_notification: F,
) -> io::Result<crate::rpc::Response>
where
    F: FnMut(&str),
    C: Fn() -> bool,
{
    let token = read_token(config)?;
    let mut stream = acquire(config)?;
    let line = request_line(method, params, token);
    ipc::write_frame_limit(&mut stream, &line, ipc::LOCAL_FRAME_MAX)?;

    let polling = idle.is_some();
    let mut reader = ipc::FrameReader::new(BufReader::new(PollReader { stream, polling }));
    let bound = idle.map(ReadBound::Idle).unwrap_or(ReadBound::Unbounded);
    let response = read_response(&mut reader, bound, cancelled, on_notification)?;
    release(config, reader.into_inner().into_inner().into_inner());
    Ok(response)
}

/// The one control-channel request envelope. The local channel always uses id 1
/// and carries the management token; only the read side varies by budget.
fn request_line(method: &str, params: serde_json::Value, token: String) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
        "auth": token,
    })
    .to_string()
}

/// Parse one response frame. `None` is an interim notification (no id) that the
/// caller may forward or ignore; `Some` is the final response.
fn response_from_line(line: &str) -> io::Result<Option<crate::rpc::Response>> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if value.get("method").is_some() && value.get("id").is_none() {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// How long one control read waits before the caller re-checks its deadline
/// and the disconnect token. It bounds both the 504 deadline and disconnect
/// cancellation to one poll period.
const CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A control-stream read that never blocks. The readiness wait belongs to the
/// deadline loop; without this adapter `BufReader::fill_buf` does a second
/// blocking read after a partial frame and ignores both the deadline and a
/// disconnect. A zero-timeout poll (Unix) or peek (Windows) turns that read
/// into `WouldBlock` while `FrameReader` keeps the bytes it already has.
struct PollReader {
    stream: Stream,
    /// `true` turns a read into a zero-timeout readiness check. The CLI keeps
    /// `false`: without a deadline there is nothing to wake up for, and the
    /// blocking read avoids a poll period of added latency.
    polling: bool,
}

impl PollReader {
    fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        self.stream.wait_readable(timeout)
    }

    fn into_inner(self) -> Stream {
        self.stream
    }
}

impl std::io::Read for PollReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.polling && !self.stream.wait_readable(Duration::ZERO)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "no control data ready",
            ));
        }
        std::io::Read::read(&mut self.stream, buf)
    }
}

/// What bounds one control read.
#[derive(Clone, Copy)]
enum ReadBound {
    /// No deadline: a plain blocking frame read (the CLI). Without a bound the
    /// cancellation flag is not observed, because there is no poll to check it.
    Unbounded,
    /// Fail when no frame arrives for this long; every frame resets it.
    Idle(Duration),
    /// Fail at this absolute instant.
    Deadline(Instant),
}

/// One frame loop shared by the streaming and the deadline-bound control reads.
/// Notifications are forwarded to `on_notification`; `request_within` passes a
/// sink that drops them because management routes do not consume interim
/// frames, but both share the frame parser so a protocol change cannot drift.
fn read_response<C, F>(
    reader: &mut ipc::FrameReader<BufReader<PollReader>>,
    bound: ReadBound,
    cancelled: C,
    mut on_notification: F,
) -> io::Result<crate::rpc::Response>
where
    C: Fn() -> bool,
    F: FnMut(&str),
{
    let mut last_frame = Instant::now();
    loop {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "control request cancelled",
            ));
        }
        let now = Instant::now();
        let wait = match bound {
            // The CLI path has no deadline and no cancellation to observe, so
            // it keeps the old blocking read instead of waking every poll.
            ReadBound::Unbounded => None,
            ReadBound::Idle(idle) => {
                let elapsed = now.saturating_duration_since(last_frame);
                if elapsed >= idle {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "control request timed out",
                    ));
                }
                Some((idle - elapsed).min(CONTROL_POLL_INTERVAL))
            }
            ReadBound::Deadline(deadline) => {
                if now >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "control request timed out",
                    ));
                }
                Some(
                    deadline
                        .saturating_duration_since(now)
                        .min(CONTROL_POLL_INTERVAL),
                )
            }
        };
        if let Some(wait) = wait {
            if !reader.has_buffered() && !reader.get_ref().get_ref().wait_readable(wait)? {
                continue;
            }
        }
        match reader.read_frame_limit(ipc::LOCAL_FRAME_MAX) {
            Ok(Some(line)) => {
                last_frame = Instant::now();
                let Some(response) = response_from_line(&line)? else {
                    on_notification(&line);
                    continue;
                };
                return Ok(response);
            }
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "empty response frame",
                ))
            }
            Err(ipc::FrameError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                // A partial frame is already buffered: let the socket wake the
                // next poll instead of spinning on `WouldBlock`.
                let _ = reader
                    .get_ref()
                    .get_ref()
                    .wait_readable(CONTROL_POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// One request/response round trip with a deadline and a cancellation check.
///
/// The blocking `request` stays the CLI path: a long-running service action is
/// not a hung supervisor, and the CLI connection pool keeps its semantics. A
/// server-side management route calls this instead, so a supervisor that never
/// answers releases the blocking worker and the connection at `deadline`.
///
/// `cancelled` is checked once per short poll. A timed-out or cancelled socket
/// is dropped, never returned to the pool: a late response would otherwise be
/// mistaken for the next request's reply.
pub fn request_within<C: Fn() -> bool>(
    config: &Path,
    method: &str,
    params: serde_json::Value,
    deadline: Instant,
    cancelled: C,
) -> io::Result<crate::rpc::Response> {
    let token = read_token(config)?;
    let mut stream = acquire(config)?;
    let line = request_line(method, params, token);
    ipc::write_frame_limit(&mut stream, &line, ipc::LOCAL_FRAME_MAX)?;

    let mut reader = ipc::FrameReader::new(BufReader::new(PollReader {
        stream,
        polling: true,
    }));
    let response = read_response(
        &mut reader,
        ReadBound::Deadline(deadline),
        cancelled,
        |_| {},
    )?;
    release(config, reader.into_inner().into_inner().into_inner());
    Ok(response)
}

pub fn remove_socket(config: &Path) {
    #[cfg(unix)]
    {
        let _ = fs::remove_file(config.join("svcd.sock"));
    }
    #[cfg(not(unix))]
    {
        let _ = config;
    }
}

#[cfg(unix)]
mod imp {
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    #[allow(dead_code)]
    pub struct Listener(UnixListener);

    pub struct Stream(UnixStream);

    #[allow(dead_code)]
    impl Listener {
        pub fn bind(config: &Path) -> io::Result<Self> {
            let path = config.join("svcd.sock");
            if path.exists() {
                if UnixStream::connect(&path).is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        "svcd control socket is already in use",
                    ));
                }
                let _ = fs::remove_file(&path);
            }
            let listener = UnixListener::bind(&path)?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o660))?;
            Ok(Listener(listener))
        }

        pub fn incoming(&self) -> impl Iterator<Item = io::Result<Stream>> + '_ {
            self.0.incoming().map(|stream| stream.map(Stream))
        }
    }

    impl Stream {
        /// Wait until the socket is readable (or the timeout expires).
        ///
        /// The pooled stream keeps its blocking mode: poll decides when a read
        /// is safe, so a deadline never has to mutate the socket that the next
        /// request will reuse.
        pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
            let mut descriptor = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut descriptor, 1, millis) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(false);
                }
                return Err(error);
            }
            Ok(ready > 0)
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

    pub fn connect(config: &Path) -> io::Result<Stream> {
        UnixStream::connect(config.join("svcd.sock")).map(Stream)
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::hash::{Hash, Hasher};
    use std::io::{self, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::path::Path;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, WaitNamedPipeW,
    };

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

    pub struct Listener {
        name: Vec<u16>,
    }

    pub struct Stream(File);

    pub struct Incoming {
        name: Vec<u16>,
    }

    impl Listener {
        pub fn bind(config: &Path) -> io::Result<Self> {
            let name = wide(&pipe_name(config));
            if pipe_exists(&name) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "svcd control pipe is already in use",
                ));
            }
            Ok(Listener { name })
        }

        pub fn incoming(&self) -> Incoming {
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

    impl Stream {
        /// Wait until the pipe has bytes (or the timeout expires).
        ///
        /// A byte-mode pipe opened without `FILE_FLAG_OVERLAPPED` has no read
        /// timeout: a plain `ReadFile` would block until the supervisor
        /// answers. Peeking first makes the blocking read safe: it only runs
        /// when at least one byte is already available, so the loop can check
        /// its deadline and the disconnect token between peeks.
        pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
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

    pub fn connect(config: &Path) -> io::Result<Stream> {
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

    fn pipe_name(config: &Path) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        config.to_string_lossy().hash(&mut hasher);
        format!(r"\\.\pipe\svcd-{:016x}", hasher.finish())
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }
}


#[cfg(test)]
mod tests {
    use super::imp::Listener;
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn config_dir(tag: &str) -> PathBuf {
        let directory = crate::test_support::temp_dir().join(format!(
            "svcd-channel-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("config dir");
        fs::write(directory.join("svcd.token"), "test-token\n").expect("token");
        directory
    }

    fn listener(config: &Path) -> Listener {
        Listener::bind(config).expect("listener")
    }

    /// A supervisor that accepts one connection and never answers.
    fn silent_server(listener: Listener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _stream = listener
                .incoming()
                .next()
                .expect("accept")
                .expect("stream");
            std::thread::sleep(Duration::from_secs(2));
        })
    }

    /// A supervisor that never answers must not hold the worker: the request
    /// returns TimedOut at the deadline and the socket is not pooled.
    #[test]
    fn a_silent_control_channel_hits_the_deadline() {
        let config = config_dir("silent");
        let server = silent_server(listener(&config));

        let started = Instant::now();
        let error = request_within(
            &config,
            "svcd.ping",
            serde_json::json!({}),
            Instant::now() + Duration::from_millis(150),
            || false,
        )
        .expect_err("the silent supervisor must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the deadline must bound the round trip, took {:?}",
            started.elapsed()
        );
        assert!(
            pool()
                .lock()
                .expect("pool")
                .get(&config)
                .map(Vec::is_empty)
                .unwrap_or(true),
            "a timed-out connection must not be reused"
        );
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    /// A flipped disconnect token cancels within one poll period, well before
    /// the deadline.
    #[test]
    fn a_disconnect_cancels_within_one_poll_period() {
        let config = config_dir("cancel");
        let server = silent_server(listener(&config));

        let started = Instant::now();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flip = cancelled.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flip.store(true, Ordering::Release);
        });
        let error = request_within(
            &config,
            "svcd.ping",
            serde_json::json!({}),
            Instant::now() + Duration::from_secs(5),
            || cancelled.load(Ordering::Acquire),
        )
        .expect_err("a cancelled request must not wait for the deadline");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancel must arrive in one poll period, took {:?}",
            started.elapsed()
        );
        flipper.join().expect("flipper");
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    /// A response that arrives in two chunks must survive the poll timeout:
    /// the first chunk is kept, not discarded as a desynchronized stream.
    #[test]
    fn a_partial_response_survives_the_poll_timeout() {
        let config = config_dir("partial");
        let listener = listener(&config);
        let server = std::thread::spawn(move || {
            let mut stream = listener
                .incoming()
                .next()
                .expect("accept")
                .expect("stream");
            stream
                .write_all(br#"{"jsonrpc":"2.0","id":1,"#)
                .expect("first chunk");
            stream.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(100));
            stream
                .write_all(br#""result":{"ok":true}}"#)
                .expect("second chunk");
            stream.write_all(b"\n").expect("newline");
            stream.flush().expect("flush");
        });

        let response = request_within(
            &config,
            "svcd.ping",
            serde_json::json!({}),
            Instant::now() + Duration::from_secs(2),
            || false,
        )
        .expect("the resumed frame is a response");
        assert_eq!(response.result, Some(serde_json::json!({ "ok": true })));
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    /// A half frame followed by silence must not park the worker in a blocking
    /// read: the short read timeout has to keep the deadline and the cancel
    /// token observable even with partial bytes already read.
    #[test]
    fn a_stalled_partial_frame_still_hits_the_deadline() {
        let config = config_dir("stalled-partial");
        let listener = listener(&config);
        let server = std::thread::spawn(move || {
            let mut stream = listener
                .incoming()
                .next()
                .expect("accept")
                .expect("stream");
            stream
                .write_all(br#"{"jsonrpc":"2.0","id":1,"#)
                .expect("partial frame");
            stream.flush().expect("flush");
            std::thread::sleep(Duration::from_secs(2));
        });

        let started = Instant::now();
        let error = request_within(
            &config,
            "svcd.ping",
            serde_json::json!({}),
            Instant::now() + Duration::from_millis(150),
            || false,
        )
        .expect_err("a partial frame cannot extend the deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the deadline must fire with a partial frame, took {:?}",
            started.elapsed()
        );
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    fn pooled(config: &Path) -> usize {
        pool()
            .lock()
            .expect("pool")
            .get(config)
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// A notification split across two writes must be forwarded exactly once,
    /// and the completed round trip must return to the pool, like every other
    /// CLI control request.
    #[test]
    fn a_stream_request_forwards_a_split_notification_before_the_response() {
        let config = config_dir("stream-notify");
        let listener = listener(&config);
        let server = std::thread::spawn(move || {
            let mut stream = listener
                .incoming()
                .next()
                .expect("accept")
                .expect("stream");
            stream
                .write_all(br#"{"jsonrpc":"2.0","method":"notifications/progress","#)
                .expect("first chunk");
            stream.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(80));
            stream
                .write_all(br#""params":{"progress":1}}"#)
                .expect("second chunk");
            stream.write_all(b"\n").expect("newline");
            stream
                .write_all(br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#)
                .expect("response");
            stream.write_all(b"\n").expect("newline");
            stream.flush().expect("flush");
        });

        let mut notifications = Vec::new();
        let response = request_stream(
            &config,
            "registry.call",
            serde_json::json!({}),
            None,
            || false,
            |line| notifications.push(line.to_string()),
        )
        .expect("the response arrives after the notification");
        assert_eq!(response.result, Some(serde_json::json!({ "ok": true })));
        assert_eq!(notifications.len(), 1, "{notifications:?}");
        assert!(
            notifications[0].contains("\"progress\":1"),
            "{notifications:?}"
        );
        assert_eq!(
            pooled(&config),
            1,
            "a completed stream returns its socket to the pool"
        );
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    /// A silent supervisor must not hold the caller past the idle bound.
    #[test]
    fn a_stream_request_without_a_frame_hits_its_idle_deadline() {
        let config = config_dir("stream-idle");
        let server = silent_server(listener(&config));

        let started = Instant::now();
        let error = request_stream(
            &config,
            "registry.call",
            serde_json::json!({}),
            Some(Duration::from_millis(150)),
            || false,
            |_| {},
        )
        .expect_err("a silent stream must hit the idle deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the idle bound must not wait for the final frame, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            pooled(&config),
            0,
            "a timed-out connection must not be reused"
        );
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    /// A disconnect check that flips must stop the read within one poll period,
    /// and the half-finished request must not poison the pool. On Windows this
    /// is the interruptible-pipe unit test: the read is parked on a silent
    /// `PeekNamedPipe`, and the cancel flag must return it without new data.
    #[test]
    fn a_cancelled_stream_request_drops_the_socket_instead_of_pooling_it() {
        let config = config_dir("stream-cancel");
        let server = silent_server(listener(&config));

        let cancelled = Arc::new(AtomicBool::new(false));
        let flip = cancelled.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flip.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let error = request_stream(
            &config,
            "registry.call",
            serde_json::json!({}),
            Some(Duration::from_secs(5)),
            || cancelled.load(Ordering::Acquire),
            |_| {},
        )
        .expect_err("a cancelled stream must not wait for the final frame");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancel must arrive in one poll period, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            pooled(&config),
            0,
            "a socket with a late reply must not be reused"
        );
        flipper.join().expect("flipper");
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }

    /// The Windows control pipe has no read timeout: the peek that precedes a
    /// short read must report "no data" without blocking, or the deadline and
    /// the disconnect check above could never run.
    #[cfg(windows)]
    #[test]
    fn the_windows_pipe_peek_does_not_wait_for_data() {
        let config = config_dir("pipe-peek");
        let server = silent_server(listener(&config));

        let stream = connect(&config).expect("connect");
        let started = Instant::now();
        assert!(
            !stream.wait_readable(Duration::ZERO).expect("peek"),
            "a silent pipe must not report readable"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the peek must not wait for data, took {:?}",
            started.elapsed()
        );
        drop(stream);
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }
}
