use std::collections::HashMap;
use std::fs;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(not(unix))]
pub use imp::Listener;
pub use imp::Stream;

use crate::ipc;
#[cfg(not(unix))]
use crate::rpc;
#[cfg(not(unix))]
use crate::threads;

pub struct PendingCall {
    pub line: String,
    pub reply: Reply,
}

/// A response handle for one management request. The daemon sends the serialized
/// response on the channel; a reactor-backed server also gets a byte on a
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

/// Bind the local control channel. Unix serves all connections from one poll
/// reactor; Windows keeps the thread-per-connection fallback.
#[cfg(unix)]
pub fn spawn_server(
    config: &Path,
    tx: Sender<PendingCall>,
    wake: std::sync::Arc<crate::wake::Wake>,
) -> io::Result<()> {
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
    crate::channel_reactor::spawn(listener, tx, wake)
}

#[cfg(not(unix))]
pub fn spawn_server(
    config: &Path,
    tx: Sender<PendingCall>,
    wake: std::sync::Arc<crate::wake::Wake>,
) -> io::Result<()> {
    let listener = Listener::bind(config)?;
    std::thread::Builder::new()
        .name("svcd-channel".into())
        .stack_size(threads::IO_STACK)
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
                    .stack_size(threads::IO_STACK)
                    .spawn(move || serve(stream, tx, wake));
            }
        })
        .map(|_| ())
}

#[cfg(not(unix))]
fn serve(stream: Stream, tx: Sender<PendingCall>, wake: std::sync::Arc<crate::wake::Wake>) {
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
    request_stream(config, method, params, |_| {})
}

/// A request whose handler may emit JSON-RPC notifications before the final
/// response. Notifications are delivered to `on_notification`; reading stops at
/// the response with an `id`.
pub fn request_stream<F>(
    config: &Path,
    method: &str,
    params: serde_json::Value,
    mut on_notification: F,
) -> io::Result<crate::rpc::Response>
where
    F: FnMut(&str),
{
    let token = read_token(config)?;
    let mut stream = acquire(config)?;
    let line = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
        "auth": token,
    })
    .to_string();
    ipc::write_frame_limit(&mut stream, &line, ipc::LOCAL_FRAME_MAX)?;

    let mut reader = BufReader::new(stream);
    loop {
        let line = ipc::read_frame_limit(&mut reader, ipc::LOCAL_FRAME_MAX)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "empty response frame"))?;
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if value.get("method").is_some() && value.get("id").is_none() {
            on_notification(&line);
            continue;
        }
        let response = serde_json::from_value(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        release(config, reader.into_inner());
        return Ok(response);
    }
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
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;

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
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::path::Path;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::CreateFileW;
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, WaitNamedPipeW};

    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
    const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
    const PIPE_WAIT: u32 = 0x0000_0000;
    const PIPE_UNLIMITED_INSTANCES: u32 = 255;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const OPEN_EXISTING: u32 = 3;
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_PIPE_BUSY: u32 = 231;
    const ERROR_PIPE_CONNECTED: u32 = 535;

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
