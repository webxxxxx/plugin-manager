//! The local control channel: CLI/MCP clients ↔ the supervisor.
//!
//! This file is the platform-neutral half: the request envelope, the
//! connection pool and the response reader. The two platform halves live in
//! `channel/platform/{unix,windows}.rs` and implement [`ControlChannel`]; this
//! file never names a platform, so the seam is one alias (`Platform`) rather
//! than a `cfg` per function.
//!
//! The server half differs by host only in how the composition root hands the
//! bound listener to its event loop: Unix gives it to the server loop, Windows
//! starts the thread-per-connection fallback itself. [`bind_server`] is the one
//! call the daemon makes; the platform decides where the accept loop lives.

mod platform;

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::wire::message::ipc;
use crate::platform::host::wake::Wake;

use platform::Platform;
pub(crate) use platform::{ControlBinding, Notify, Stream};

/// One message from an outbound streaming management call. The control
/// channel owns this shape; the MCP session consumes it.
#[allow(dead_code)]
#[derive(Debug)]
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
    /// The endpoint the connection came in on; the daemon tiers on it.
    pub endpoint: Endpoint,
}

/// A response handle for one management request. The daemon sends the serialized
/// response on the channel; the Unix server loop also gets a byte on a
/// notifier pipe so it can wake immediately instead of polling.
#[derive(Clone)]
pub struct Reply {
    sender: Sender<String>,
    notify: Option<Notify>,
}

impl Reply {
    #[allow(dead_code)]
    pub fn plain(sender: Sender<String>) -> Self {
        Reply {
            sender,
            notify: None,
        }
    }

    #[allow(dead_code)]
    pub fn with_notify(sender: Sender<String>, notify: Notify) -> Self {
        Reply {
            sender,
            notify: Some(notify),
        }
    }

    pub fn send(&self, response: String) {
        if self.sender.send(response).is_ok() {
            if let Some(notify) = &self.notify {
                notify.wake();
            }
        }
    }
}

/// One control-channel implementation. There are two real implementations
/// (UDS / named pipe); only the host's compiles. The interface covers the whole
/// platform decision the channel makes: bind, serve-or-handoff, connect and
/// socket cleanup.
pub(crate) trait ControlChannel {
    /// The connected byte stream used by clients and by one accepted server
    /// connection.
    type Stream: ControlStream;
    /// The bound server socket/pipe.
    type Listener: ControlListener<Stream = Self::Stream>;
    /// What `serve` returns to the composition root.
    type Server;

    /// Bind one of the two server endpoints.
    fn bind(config: &Path, endpoint: Endpoint) -> io::Result<Self::Listener>;

    /// Turn both bound endpoints into the server the composition root will run:
    /// the carrier itself on a poll host, one started accept thread per
    /// endpoint otherwise.
    fn into_server(
        user: Self::Listener,
        admin: Self::Listener,
        pending: SyncSender<PendingCall>,
        wake: Arc<Wake>,
        io_stack: usize,
    ) -> io::Result<Self::Server>;

    /// Connect a client to the endpoint its method belongs to.
    fn connect(config: &Path, endpoint: Endpoint) -> io::Result<Self::Stream>;

    /// Remove the endpoint's filesystem entry, if it has one.
    fn remove_socket(config: &Path);
}

/// The accept side of a bound endpoint.
pub(crate) trait ControlListener {
    type Stream: ControlStream;

    fn incoming(&self) -> impl Iterator<Item = io::Result<Self::Stream>> + '_;
}

/// The stream operations the request loop needs beyond `Read`/`Write`.
pub(crate) trait ControlStream: Read + Write + Send + 'static {
    /// Wait until the stream is readable (or the timeout expires).
    fn wait_readable(&self, timeout: Duration) -> io::Result<bool>;
}

/// Which control endpoint a request or connection belongs to.
///
/// The split is enforced twice: the OS checks the endpoint's ACL when a client
/// connects, and the daemon checks the tier again so a connection can never
/// carry a method its endpoint was not granted (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// Read + lifecycle: the `svcd` socket/pipe, reachable by the `svcd` group.
    User,
    /// Write: the `svcd-admin` socket/pipe, reachable by root/Administrators.
    Admin,
}

impl Endpoint {
    /// §6: the group tier may read and drive lifecycle; every other method is
    /// admin. Unknown methods fail closed onto the admin endpoint.
    pub fn for_method(method: &str) -> Endpoint {
        match method {
            "svcd.ping" | "svcd.compat" | "svcd.reload" | "registry.tools" | "tools/list"
            | "registry.version" | "registry.call" | "tools/call" | "svc.list" | "services"
            | "svc.status" | "svc.start" | "svc.stop" | "svc.restart" | "svc.reload"
            | "auth.status" => Endpoint::User,
            _ => Endpoint::Admin,
        }
    }
}

/// Bind and start the local control channel. Unix hands the listener to the
/// server loop; Windows keeps the thread-per-connection fallback. This is the
/// one call the daemon makes on every host.
pub fn bind_server(
    config: &Path,
    pending: SyncSender<PendingCall>,
    wake: Arc<Wake>,
    io_stack: usize,
) -> io::Result<ControlBinding> {
    let user = Platform::bind(config, Endpoint::User)?;
    let admin = match Platform::bind(config, Endpoint::Admin) {
        Ok(admin) => admin,
        Err(error) => {
            Platform::remove_socket(config);
            return Err(error);
        }
    };
    Platform::into_server(user, admin, pending, wake, io_stack)
}

const POOL_MAX: usize = 8;

fn pool() -> &'static Mutex<HashMap<(PathBuf, Endpoint), Vec<Stream>>> {
    static POOL: OnceLock<Mutex<HashMap<(PathBuf, Endpoint), Vec<Stream>>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn acquire(config: &Path, endpoint: Endpoint) -> io::Result<Stream> {
    let key = (config.to_path_buf(), endpoint);
    if let Ok(mut pool) = pool().lock() {
        if let Some(stream) = pool.get_mut(&key).and_then(Vec::pop) {
            return Ok(stream);
        }
    }
    connect(config, endpoint)
}

fn release(config: &Path, endpoint: Endpoint, stream: Stream) {
    if let Ok(mut pool) = pool().lock() {
        let connections = pool.entry((config.to_path_buf(), endpoint)).or_default();
        if connections.len() < POOL_MAX {
            connections.push(stream);
        }
    }
}

pub fn connect(config: &Path, endpoint: Endpoint) -> io::Result<Stream> {
    Platform::connect(config, endpoint)
}

/// One request/response round trip on the local management channel.
pub fn request(
    config: &Path,
    method: &str,
    params: serde_json::Value,
) -> io::Result<crate::wire::message::rpc::Response> {
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
) -> io::Result<crate::wire::message::rpc::Response>
where
    F: FnMut(&str),
    C: Fn() -> bool,
{
    let endpoint = Endpoint::for_method(method);
    let mut stream = acquire(config, endpoint)?;
    let line = request_line(method, params);
    ipc::write_frame_limit(&mut stream, &line, ipc::LOCAL_FRAME_MAX)?;

    let polling = idle.is_some();
    let mut reader = ipc::FrameReader::new(BufReader::new(PollReader { stream, polling }));
    let bound = idle.map(ReadBound::Idle).unwrap_or(ReadBound::Unbounded);
    let response = read_response(&mut reader, bound, cancelled, on_notification)?;
    release(
        config,
        endpoint,
        reader.into_inner().into_inner().into_inner(),
    );
    Ok(response)
}

/// The one control-channel request envelope. The local channel always uses id 1
/// and carries no credential: authorization is the endpoint's ACL.
fn request_line(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    })
    .to_string()
}

/// Parse one response frame. `None` is an interim notification (no id) that the
/// caller may forward or ignore; `Some` is the final response.
fn response_from_line(line: &str) -> io::Result<Option<crate::wire::message::rpc::Response>> {
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
) -> io::Result<crate::wire::message::rpc::Response>
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
) -> io::Result<crate::wire::message::rpc::Response> {
    let endpoint = Endpoint::for_method(method);
    let mut stream = acquire(config, endpoint)?;
    let line = request_line(method, params);
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
    release(
        config,
        endpoint,
        reader.into_inner().into_inner().into_inner(),
    );
    Ok(response)
}

/// Remove the endpoint after the daemon stops, if the host keeps one on disk.
pub fn remove_socket(config: &Path) {
    Platform::remove_socket(config);
}

#[cfg(test)]
mod tests {
    use super::platform::Platform;
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    type Listener = <Platform as ControlChannel>::Listener;

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
        directory
    }

    fn listener(config: &Path) -> Listener {
        Platform::bind(config, Endpoint::User).expect("listener")
    }

    /// A supervisor that accepts one connection and never answers.
    fn silent_server(listener: Listener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let _stream = listener.incoming().next().expect("accept").expect("stream");
            std::thread::sleep(Duration::from_secs(2));
        })
    }

    /// The host's implementation must satisfy the one control-channel seam.
    #[test]
    fn the_host_control_channel_implements_the_seam() {
        fn assert_channel<C: ControlChannel>() {}
        assert_channel::<Platform>();
    }

    /// §6: the group tier reads and drives lifecycle; every other method is
    /// admin, and unknown methods fail closed.
    #[test]
    fn methods_are_tiered_by_section_six() {
        for method in [
            "svcd.ping",
            "svc.list",
            "svc.status",
            "svc.start",
            "svc.stop",
            "svc.restart",
            "registry.tools",
            "registry.call",
        ] {
            assert_eq!(Endpoint::for_method(method), Endpoint::User, "{method}");
        }
        for method in [
            "plugin.install",
            "plugin.uninstall",
            "svc.enable",
            "svc.disable",
            "auth.passwd",
            "svcd.shutdown",
            "an.unknown.method",
        ] {
            assert_eq!(Endpoint::for_method(method), Endpoint::Admin, "{method}");
        }
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
        assert_eq!(
            pooled(&config),
            0,
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
            let mut stream = listener.incoming().next().expect("accept").expect("stream");
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
            let mut stream = listener.incoming().next().expect("accept").expect("stream");
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
            .iter()
            .filter(|((path, _), _)| path.as_path() == config)
            .map(|(_, streams)| streams.len())
            .sum()
    }

    /// A notification split across two writes must be forwarded exactly once,
    /// and the completed round trip must return to the pool, like every other
    /// CLI control request.
    #[test]
    fn a_stream_request_forwards_a_split_notification_before_the_response() {
        let config = config_dir("stream-notify");
        let listener = listener(&config);
        let server = std::thread::spawn(move || {
            let mut stream = listener.incoming().next().expect("accept").expect("stream");
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

    /// A silent control stream must report "no data" without blocking, or the
    /// deadline and the disconnect check above could never run. The pipe peek
    /// and the socket poll must agree on this.
    #[test]
    fn an_idle_control_stream_reports_no_data() {
        let config = config_dir("idle-stream");
        let server = silent_server(listener(&config));

        let stream = connect(&config, Endpoint::User).expect("connect");
        let started = Instant::now();
        assert!(
            !stream.wait_readable(Duration::ZERO).expect("poll"),
            "a silent stream must not report readable"
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the readiness check must not wait for data, took {:?}",
            started.elapsed()
        );
        drop(stream);
        server.join().expect("server");
        let _ = fs::remove_dir_all(&config);
    }
}
