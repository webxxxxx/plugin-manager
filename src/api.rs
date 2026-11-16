//! Minimal `/api/v1` HTTP management API and authentication.
//!
//! `bind` in `svcd.conf` chooses the listener address. A non-loopback bind
//! requires `auth_passwd`; without it the daemon falls back to loopback so the
//! operator can still run `svc auth passwd`. Every request is translated into
//! the same daemon RPC the CLI uses, so the HTTP surface never owns a second
//! copy of service state.

#[cfg(unix)]
use crate::channel_client_reactor::ClientReactor;
#[cfg(any(not(unix), test))]
use std::collections::HashMap;
#[cfg(unix)]
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::mpsc::{self, Receiver, Sender};
#[cfg(unix)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use serde_json::{json, Value};

use crate::blob;
use crate::blob_http;
use crate::channel;
use crate::face::plugin_ui::{ui_path, UiPath};
use crate::http;
use crate::lan::{self, AuthKind, Lan, LanStatus};
use crate::log;
#[cfg(not(unix))]
use crate::mcp;
use crate::mcp_http;
use crate::package;
use crate::paths;
use crate::plugin;
use crate::rpc;
use crate::server::http::engine::{self, keep_alive_response, wants_keep_alive, HttpRequest};
#[cfg(unix)]
use crate::server::http::JobOutcome;
use crate::settings;
use crate::threads;
use crate::transfer;
use crate::webui;

const UI: &str = include_str!("web/index.html");

/// Buffered-body limit for the routes that still go through the ordinary
/// router. Blob uploads/downloads are streamed before this is consulted.
fn body_limit(path: &str) -> u64 {
    let limits = crate::limits::current();
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    if path.starts_with("/api/v1/plugins") || path.starts_with("/api/v1/import") {
        limits.max_body
    } else if path.trim_end_matches('/').ends_with("/complete")
        && (path.starts_with("/api/v1/blobs/uploads/")
            || path.starts_with("/api/v1/file/uploads/")
            || path.starts_with("/api/v1/files/uploads/"))
    {
        limits.max_complete_body
    } else {
        limits.max_json_body
    }
}

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Serialise config writes inside this process. The on-disk `config.lock`
/// covers other processes on Unix; this mutex also covers Windows, where
/// `flock` is a no-op, and keeps two HTTP threads from sharing the same
/// `config.tmp.<pid>` path.
static CONFIG_WRITES: Mutex<()> = Mutex::new(());

pub struct Api {
    lan: Arc<Mutex<Lan>>,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}

fn resolve_bind(env: Option<&str>, configured: Option<SocketAddr>) -> SocketAddr {
    env.and_then(|value| value.parse::<SocketAddr>().ok())
        .or(configured)
        .unwrap_or_else(default_bind)
}

pub fn spawn(config: PathBuf, raw_hub: Arc<crate::rawhub::RawHub>) -> std::io::Result<Arc<Api>> {
    let configured = settings::current().ok().and_then(|settings| settings.bind);
    let requested = resolve_bind(std::env::var("SVCD_BIND").ok().as_deref(), configured);
    let password_hash = settings::current()
        .ok()
        .and_then(|settings| settings.auth_passwd.clone());

    // A LAN bind without a password would expose an unauthenticated API. Keep
    // the daemon reachable on loopback so the operator can still run
    // `svc auth passwd`, then restart to pick up the requested bind.
    let mut bind = requested;
    if !requested.ip().is_loopback() && password_hash.is_none() {
        log::svcd(&format!(
            "svcd: bind {} requires auth_passwd; falling back to 127.0.0.1:{}",
            requested,
            requested.port()
        ));
        bind = SocketAddr::from(([127, 0, 0, 1], requested.port()));
    }
    let require_auth = !bind.ip().is_loopback();
    let mut state = Lan::new(bind, requested, require_auth, password_hash);
    let listener = TcpListener::bind(bind)?;
    let local = listener.local_addr()?;
    state.bind = local;
    let port_file = config.join("http.port");
    fs::write(&port_file, format!("127.0.0.1:{}", local.port())).map_err(|error| {
        std::io::Error::other(format!("write {}: {}", port_file.display(), error))
    })?;
    log::svcd(&format!(
        "svcd: HTTP API on {} (auth required: {})",
        local, require_auth
    ));

    let lan = Arc::new(Mutex::new(state));
    let api = Arc::new(Api { lan: lan.clone() });
    // The reactor parses request heads and prepares the takeover routes (plugin
    // UI streams load the plugin manifest), which is the same class of work
    // transfer_stack exists for. io_stack (32 KiB floor) and request_stack
    // (64 KiB floor) both overflowed there under a minimal svcd.conf and
    // aborted the whole daemon (#108).
    #[cfg(unix)]
    legacy::start(listener, lan, config, raw_hub)?;
    #[cfg(not(unix))]
    {
        let _ = raw_hub;
        thread::Builder::new()
            .name("svcd-http".into())
            .stack_size(threads::io_stack())
            .spawn(move || listener_loop(Some(listener), lan, config))?;
    }
    Ok(api)
}

impl Api {
    pub fn set_password(&self, hash: String) -> Result<LanStatus, String> {
        let mut state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        state.set_password(hash);
        Ok(state.status())
    }

    pub fn status(&self) -> Result<LanStatus, String> {
        let state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        Ok(state.status())
    }
}

#[cfg(unix)]
struct HttpJob {
    connection: u64,
    request: HttpRequest,
    keep_alive: bool,
}

#[cfg(unix)]
enum Work {
    Http(HttpJob),
}

#[cfg(unix)]
struct JobQueue {
    queue: Mutex<VecDeque<Work>>,
    ready: Condvar,
    /// Maximum queued legacy jobs. The legacy bridge answers 503 instead of
    /// growing without bound.
    limit: usize,
}

#[cfg(unix)]
struct JobResult {
    connection: u64,
    response: Vec<u8>,
    keep_alive: bool,
}

#[cfg(unix)]
enum WorkResult {
    Http(JobResult),
    /// One blocking-pool job ended; the server loop takes its completion slot
    /// by connection id, or drops it when the connection is gone.
    BlockingDone {
        key: u64,
        outcome: JobOutcome,
    },
    /// A stream handler's blocking job produced a value. The handler receives
    /// it before the pool's own completion frees the key.
    StreamJobDone {
        connection: u64,
        outcome: crate::server::http::BlockingOutcome,
    },
}

#[cfg(unix)]
enum ConnState {
    Reading,
    Processing,
    Writing,
    /// A sans-IO handler plus the server-side deadline, file send and
    /// `max_streams` slot (see `server::http::driver`).
    Stream(crate::server::http::driver::StreamDriver),
}

#[cfg(unix)]
type Connection = engine::Connection<ConnState>;

#[cfg(unix)]
mod legacy;

#[cfg(unix)]
impl JobQueue {
    /// Queue one legacy job, or hand it back when the bridge is full. The
    /// queue never grows past `limit` and never spawns a temporary worker.
    fn try_push(&self, work: Work) -> Result<(), Work> {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        if queue.len() >= self.limit {
            return Err(work);
        }
        queue.push_back(work);
        self.ready.notify_one();
        Ok(())
    }

    fn pop(&self) -> Work {
        let mut queue = self.queue.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(job) = queue.pop_front() {
                return job;
            }
            queue = self
                .ready
                .wait(queue)
                .unwrap_or_else(|error| error.into_inner());
        }
    }
}

#[cfg(unix)]
fn http_worker(
    queue: Arc<JobQueue>,
    config: PathBuf,
    lan: Arc<Mutex<Lan>>,
    results: Sender<WorkResult>,
    notify: Option<UnixStream>,
) {
    loop {
        let result = match queue.pop() {
            Work::Http(job) => {
                let mut response = respond(&config, &job.request, &lan);
                if job.keep_alive {
                    response = keep_alive_response(response);
                }
                WorkResult::Http(JobResult {
                    connection: job.connection,
                    response,
                    keep_alive: job.keep_alive,
                })
            }
        };
        if results.send(result).is_err() {
            return;
        }
        if let Some(notify) = &notify {
            let _ = (&*notify).write(&[1]);
        }
    }
}

#[cfg(unix)]
fn export_takeover(
    connection: &mut Connection,
) -> Option<(TcpStream, http::Head, crate::export_http::Route)> {
    if connection.pending.is_some() {
        return None;
    }
    let header_end = http::header_end(&connection.buffer)?;
    if header_end > crate::limits::current().max_header {
        return None;
    }
    let (head, _) = http::parse_head(&connection.buffer[..header_end + 4])?;
    let route = crate::export_http::route(&head)?;
    let stream = connection.stream.try_clone().ok()?;
    Some((stream, head, route))
}

#[cfg(unix)]
fn import_takeover(connection: &mut Connection) -> Option<(TcpStream, http::Head, Vec<u8>)> {
    if connection.pending.is_some() {
        return None;
    }
    let header_end = http::header_end(&connection.buffer)?;
    if header_end > crate::limits::current().max_header {
        return None;
    }
    let (head, body_start) = http::parse_head(&connection.buffer[..header_end + 4])?;
    if !crate::install_http::import_route(&head) {
        return None;
    }
    let stream = connection.stream.try_clone().ok()?;
    let initial = connection.buffer[body_start..].to_vec();
    Some((stream, head, initial))
}

#[cfg(unix)]
fn install_takeover(connection: &mut Connection) -> Option<(TcpStream, http::Head, Vec<u8>)> {
    if connection.pending.is_some() {
        return None;
    }
    let header_end = http::header_end(&connection.buffer)?;
    if header_end > crate::limits::current().max_header {
        return None;
    }
    let (head, body_start) = http::parse_head(&connection.buffer[..header_end + 4])?;
    if !crate::install_http::route(&head) {
        return None;
    }
    let stream = connection.stream.try_clone().ok()?;
    let initial = connection.buffer[body_start..].to_vec();
    Some((stream, head, initial))
}

#[cfg(unix)]
fn blob_takeover(
    connection: &mut Connection,
) -> Option<(TcpStream, http::Head, Vec<u8>, crate::blob_http::Route)> {
    if connection.pending.is_some() {
        return None;
    }
    let header_end = http::header_end(&connection.buffer)?;
    if header_end > crate::limits::current().max_header {
        return None;
    }
    let (head, body_start) = http::parse_head(&connection.buffer[..header_end + 4])?;
    let route = blob_http::route(&head)?;
    let stream = connection.stream.try_clone().ok()?;
    let initial = connection.buffer[body_start..].to_vec();
    Some((stream, head, initial, route))
}

/// Admission bookkeeping for the Windows thread-per-connection fallback.
///
/// The Unix reactor bounds its connection table directly. The fallback has no
/// such table: each accepted socket becomes a thread, so both caps have to be
/// applied *before* that thread exists. One `ConnectionLease` is exactly one
/// admitted connection and exactly one connection thread; dropping the lease
/// when the handler returns releases the global and the per-IP slot together.
///
/// Compiled in test builds on every platform so the policy can be tested
/// without a socket.
#[cfg(any(not(unix), test))]
struct Admission {
    max_connections: usize,
    max_connections_per_ip: usize,
    active: usize,
    per_ip: HashMap<IpAddr, usize>,
}

#[cfg(any(not(unix), test))]
struct ConnectionLease {
    admission: Arc<Mutex<Admission>>,
    peer: IpAddr,
}

#[cfg(any(not(unix), test))]
impl Admission {
    fn new(max_connections: usize, max_connections_per_ip: usize) -> Self {
        Self {
            max_connections,
            max_connections_per_ip,
            active: 0,
            per_ip: HashMap::new(),
        }
    }

    fn lock(admission: &Arc<Mutex<Admission>>) -> std::sync::MutexGuard<'_, Admission> {
        admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(any(not(unix), test))]
impl ConnectionLease {
    /// Reserve one connection slot for `peer`, or return `None` when either
    /// cap is already reached. A rejected connection must not consume a slot.
    fn acquire(admission: &Arc<Mutex<Admission>>, peer: IpAddr) -> Option<ConnectionLease> {
        let mut state = Admission::lock(admission);
        if state.active >= state.max_connections {
            return None;
        }
        let per_ip = state.per_ip.get(&peer).copied().unwrap_or(0);
        if per_ip >= state.max_connections_per_ip {
            return None;
        }
        state.active += 1;
        *state.per_ip.entry(peer).or_insert(0) += 1;
        Some(ConnectionLease {
            admission: Arc::clone(admission),
            peer,
        })
    }
}

#[cfg(any(not(unix), test))]
impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let mut state = Admission::lock(&self.admission);
        debug_assert!(state.active > 0, "connection lease dropped without a slot");
        state.active = state.active.saturating_sub(1);
        let remove = match state.per_ip.get_mut(&self.peer) {
            Some(count) => {
                debug_assert!(*count > 0, "connection lease dropped without a per-IP slot");
                *count = count.saturating_sub(1);
                *count == 0
            }
            None => false,
        };
        if remove {
            state.per_ip.remove(&self.peer);
        }
    }
}

#[cfg(not(unix))]
fn listener_loop(listener: Option<TcpListener>, lan: Arc<Mutex<Lan>>, config: PathBuf) {
    let Some(active) = listener.as_ref() else {
        return;
    };
    // Like the Unix reactor, snapshot the restart-time limits once. Changing
    // them in svcd.conf requires a daemon restart.
    let limits = crate::limits::current();
    let admission = Arc::new(Mutex::new(Admission::new(
        limits.max_connections,
        limits.max_connections_per_ip,
    )));
    let idle_timeout = limits.idle_timeout;
    loop {
        match active.accept() {
            Ok((stream, peer)) => {
                let ip = peer.ip();
                // Admit before spawning: a refused connection must not create
                // a thread, and an admitted one stays accounted for until its
                // handler returns and drops the lease.
                let Some(lease) = ConnectionLease::acquire(&admission, ip) else {
                    reject_connection(stream);
                    continue;
                };
                let lan = lan.clone();
                let config = config.clone();
                let spawned = thread::Builder::new()
                    .name("svcd-http-conn".into())
                    .stack_size(threads::transfer_stack())
                    .spawn(move || {
                        let _lease = lease;
                        handle(stream, ip, config, lan, idle_timeout);
                    });
                if let Err(error) = spawned {
                    // The failed spawn drops the closure, and with it the
                    // lease, so the slot is released even though no handler
                    // ever ran.
                    log::svcd(&format!("svcd: HTTP connection thread failed: {}", error));
                }
            }
            Err(error) => {
                log::svcd(&format!("svcd: HTTP accept failed: {}", error));
                return;
            }
        }
    }
}

/// Drive a face-produced stream on the Windows connection thread. The
/// fallback owns the socket and the shared max_streams slot; a Busy result is
/// rendered before any handler bytes are written.
#[cfg(not(unix))]
fn drive_fallback(stream: &mut TcpStream, handler: Box<dyn crate::server::http::StreamHandler>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
    if crate::server::http::fallback::drive(stream, handler).is_err() {
        let _ = stream.write_all(&crate::server::http::stream_busy_response());
    }
}

/// Run one plugin UI request on the Windows connection thread. The face
/// resolves the endpoint policy; the server layer's fallback driver owns the
/// socket and the handler's events.
#[cfg(not(unix))]
fn serve_plugin_ui(
    stream: &mut TcpStream,
    head: &http::Head,
    peer: IpAddr,
    lan: &Arc<Mutex<Lan>>,
    config: PathBuf,
) {
    use crate::server::http::Exchange;

    match crate::face::plugin_ui::PluginUiFace::exchange(head, peer, lan, config, None) {
        Err(response) => {
            let _ = stream.write_all(&response);
        }
        Ok(Exchange::Response(response)) => {
            let _ = stream.write_all(&response.into_bytes());
        }
        Ok(Exchange::Stream(handler)) => drive_fallback(stream, handler),
    }
}

/// Run one blob/file download through the face and the Windows fallback
/// driver. The handler owns no connection; the fallback owns the socket and
/// the shared `max_streams` slot.
#[cfg(not(unix))]
fn serve_blob_download(
    stream: &mut TcpStream,
    head: &http::Head,
    peer: IpAddr,
    face: &crate::face::api::ApiFace,
    lan: &Arc<Mutex<Lan>>,
) {
    use crate::server::http::{dispatch_inline, Exchange};

    let request = HttpRequest {
        method: head.method.clone(),
        path: head.path.clone(),
        version: head.version.clone(),
        headers: head.headers.clone(),
        body: Vec::new(),
    };
    let prepared = match prepare_face_request(face, &request, peer, lan) {
        Ok(Some(prepared)) => prepared,
        Ok(None) => {
            let _ = stream.write_all(&http::json_error(404, "not_found", "not a download route"));
            return;
        }
        Err(response) => {
            let _ = stream.write_all(&response);
            return;
        }
    };
    match dispatch_inline(face, &prepared.route, prepared.request) {
        Ok(Exchange::Response(response)) => {
            let _ = stream.write_all(&response.into_bytes());
        }
        Ok(Exchange::Stream(handler)) => drive_fallback(stream, handler),
        Err(cause) => {
            let _ = stream.write_all(&http::json_error(500, "internal", &cause.message()));
        }
    }
}

/// Refuse one accepted connection with the same busy response the Unix reactor
/// sends, instead of starting a thread that would exceed the connection cap.
#[cfg(not(unix))]
fn reject_connection(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = stream.write_all(engine::BUSY_CONNECTION_RESPONSE);
}

#[cfg(not(unix))]
fn handle(
    mut stream: TcpStream,
    peer: IpAddr,
    config: PathBuf,
    lan: Arc<Mutex<Lan>>,
    idle_timeout: Duration,
) {
    // Windows accepted sockets inherit the listener's non-blocking mode
    // (Unix ones do not). Requests are read with blocking reads, so flip the
    // socket back before installing the read timeout. Otherwise a body that has
    // not arrived yet becomes a WouldBlock "read" error.
    if let Err(error) = stream.set_nonblocking(false) {
        log::svcd(&format!(
            "svcd: HTTP connection: cannot switch to blocking mode: {}",
            error
        ));
    }
    // The configured `http_idle_timeout_secs` replaces the old hard-coded
    // 60 s. It covers any wait for request bytes: the first request, the gap
    // between keep-alive requests, and a stalled request body. That matches the
    // Unix reactor's idle sweep. The same deadline bounds a blocking write, so
    // a client that stops reading cannot pin this connection thread forever;
    // takeover handlers below install their own transfer timeout where a
    // longer transfer is expected.
    let _ = stream.set_read_timeout(Some(idle_timeout));
    let _ = stream.set_write_timeout(Some(idle_timeout));
    let face = crate::face::api::ApiFace::new(config.clone());
    loop {
        let (head, initial) = match read_head(&mut stream) {
            Ok(head) => head,
            Err(HeadFailure::Closed) => return,
            Err(HeadFailure::Response(response)) => {
                let _ = stream.write_all(&response);
                return;
            }
        };
        if crate::face::plugin_ui::PluginUiFace::route_for(&head.method, &head.path).is_some() {
            serve_plugin_ui(&mut stream, &head, peer, &lan, config.clone());
            return;
        }
        if crate::face::blob::download_target(&head.method, &head.path).is_some() {
            serve_blob_download(&mut stream, &head, peer, &face, &lan);
            return;
        }
        if let Some(route) = crate::export_http::route(&head) {
            crate::export_http::serve(&mut stream, &head, &lan, &config, route);
            return;
        }
        if crate::install_http::import_route(&head) {
            crate::install_http::serve_import(
                &mut stream,
                &head,
                &initial,
                &lan,
                &config,
                crate::limits::current(),
            );
            return;
        }
        if crate::install_http::route(&head) {
            crate::install_http::serve(
                &mut stream,
                &head,
                &initial,
                &lan,
                &config,
                crate::limits::current(),
            );
            return;
        }
        if let Some(route) = blob_http::route(&head) {
            let _ = blob_http::serve(&mut stream, &head, &initial, &lan, route);
            return;
        }
        let request = match read_body(&mut stream, head, &initial) {
            Ok(request) => request,
            Err(response) => {
                let _ = stream.write_all(&response);
                return;
            }
        };
        let keep_alive = wants_keep_alive(&request);
        if let Some(response) = serve_face(&face, &request, peer, &lan, keep_alive) {
            if stream.write_all(&response).is_err() {
                return;
            }
            if !keep_alive {
                return;
            }
            continue;
        }
        let path = request
            .path
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(request.path.as_str());
        if path == "/mcp" {
            let password_hash = lan
                .lock()
                .ok()
                .and_then(|state| state.password_hash().map(str::to_string));
            match mcp_http::prepare_streaming(
                &mcp_http::Request {
                    method: &request.method,
                    headers: &request.headers,
                    body: &request.body,
                },
                password_hash.as_deref(),
            ) {
                Ok(Some(call)) => {
                    let _ = serve_mcp_sse(&mut stream, call, &config);
                    return;
                }
                Ok(None) => {}
                Err(response) => {
                    let _ = stream.write_all(&response);
                    return;
                }
            }
        }
        let mut response = respond(&config, &request, &lan);
        if keep_alive {
            response = keep_alive_response(response);
        }
        if stream.write_all(&response).is_err() {
            return;
        }
        if !keep_alive {
            return;
        }
    }
}

#[cfg(not(unix))]
fn serve_mcp_sse(
    stream: &mut TcpStream,
    call: mcp_http::StreamingCall,
    config: &Path,
) -> std::io::Result<()> {
    // The Windows fallback has no stream driver, but the long-lived budget is
    // still server policy: reserve it before the head and before the plugin
    // call starts. The permit lives until this blocking response ends.
    let _permit = match crate::server::http::budget::StreamBudget::global().acquire(
        crate::server::http::StreamClass::LongLived,
        crate::limits::current().max_streams,
    ) {
        Ok(permit) => permit,
        Err(_) => {
            stream.write_all(&crate::server::http::stream_busy_response())?;
            return Ok(());
        }
    };
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Connection: close\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    )?;
    let mut write_error = None;
    let response = channel::request_stream(
        config,
        "registry.call",
        json!({
            "name": call.name,
            "arguments": call.arguments,
            "progressToken": call.progress_token,
        }),
        |notification| {
            if write_error.is_none() && write_sse_line(stream, notification).is_err() {
                write_error = Some(());
            }
        },
    );
    let response = match response {
        Ok(response) => mcp::finish_modern_tool_call(call.id, response),
        Err(error) => rpc::err(call.id, -32603, &format!("svcd unreachable: {}", error)),
    };
    if write_error.is_none() {
        let _ = write_sse_line(stream, &rpc::to_line(&response));
    }
    let _ = stream.write_all(b"0\r\n\r\n");
    let _ = stream.flush();
    Ok(())
}

#[cfg(not(unix))]
fn write_sse_line(stream: &mut TcpStream, line: &str) -> std::io::Result<()> {
    let payload = format!("event: message\ndata: {}\n\n", line);
    stream.write_all(format!("{:x}\r\n", payload.len()).as_bytes())?;
    stream.write_all(payload.as_bytes())?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// A face-owned request with authentication and its body already resolved.
struct PreparedFace {
    route: crate::server::http::Route,
    request: crate::server::http::Request,
}

/// Ask the face whether it owns the request, then resolve the principal and
/// apply the route's body policy. `Ok(None)` leaves the request to the legacy
/// path; `Err` is a rendered authentication or body error.
fn prepare_face_request(
    face: &dyn crate::server::http::Face,
    request: &HttpRequest,
    peer: IpAddr,
    lan: &Arc<Mutex<Lan>>,
) -> Result<Option<PreparedFace>, Vec<u8>> {
    use crate::server::http::{body_for, BodyError, Request};

    let Some(route) = face.route(&request.method, &request.path) else {
        return Ok(None);
    };
    // Authentication still runs per request here. Moving the principal to
    // connection setup belongs to the server-loop slice (#114).
    let principal = match lan.lock() {
        Ok(mut state) => match state.authenticate(&request.headers) {
            Ok(principal) => principal,
            Err(()) => return Err(error(401, "unauthorized", "authentication required")),
        },
        Err(_) => return Err(error(500, "internal", "LAN state unavailable")),
    };
    let body = match body_for(route.body, &request.body) {
        Ok(body) => body,
        Err(BodyError::TooLarge) => {
            return Err(error(413, "payload_too_large", "request body is too large"))
        }
    };
    Ok(Some(PreparedFace {
        route,
        request: Request {
            method: request.method.clone(),
            path: request.path.clone(),
            headers: request.headers.clone(),
            peer,
            principal,
            body,
        },
    }))
}

/// Turn a dispatch result into wire bytes. Streamed routes are not wired into
/// the legacy engines in this slice; #115 hands the stream handler to the
/// server loop.
fn render_exchange(
    result: Result<crate::server::http::Exchange, crate::server::http::DispatchError>,
) -> Vec<u8> {
    use crate::server::http::Exchange;

    match result {
        Ok(Exchange::Response(response)) => response.into_bytes(),
        Ok(Exchange::Stream(_)) => error(
            501,
            "not_implemented",
            "streamed face routes are not wired into this engine yet",
        ),
        Err(cause) => error(500, "internal", &cause.message()),
    }
}

/// The Windows per-connection fallback has no shared blocking pool: its
/// connection thread is already dedicated to one request, so a `Blocking`
/// route runs on that thread. The Unix server loop submits to
/// `BlockingPool` instead and never blocks the loop.
#[cfg(not(unix))]
fn serve_face(
    face: &crate::face::api::ApiFace,
    request: &HttpRequest,
    peer: IpAddr,
    lan: &Arc<Mutex<Lan>>,
    keep_alive: bool,
) -> Option<Vec<u8>> {
    use crate::server::http::{dispatch_blocking_now, dispatch_inline, Dispatch};

    let prepared = match prepare_face_request(face, request, peer, lan) {
        Ok(Some(prepared)) => prepared,
        Ok(None) => return None,
        Err(response) => {
            return Some(if keep_alive {
                keep_alive_response(response)
            } else {
                response
            })
        }
    };
    let result = match prepared.route.dispatch {
        Dispatch::Inline => dispatch_inline(face, &prepared.route, prepared.request),
        Dispatch::Blocking => dispatch_blocking_now(face, &prepared.route, prepared.request),
    };
    let mut response = render_exchange(result);
    if keep_alive {
        response = keep_alive_response(response);
    }
    Some(response)
}

fn respond(config: &Path, request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
    let path = request
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(request.path.as_str());
    if path == "/login" && request.method == "POST" {
        return login(request, lan);
    }
    if path == "/logout" && request.method == "POST" {
        return logout(request, lan);
    }
    if path == "/" || path == "/index.html" {
        if request.method != "GET" {
            return method_not_allowed("GET");
        }
        return ui_page();
    }
    if path == "/mcp" {
        // Bearer-only and outside the cookie/session policy on purpose: the
        // MCP surface must not share the browser's CSRF model (see #40).
        let password_hash = match lan.lock() {
            Ok(state) => state.password_hash().map(str::to_string),
            Err(_) => return error(500, "internal", "auth state unavailable"),
        };
        return mcp_http::handle(
            config,
            &mcp_http::Request {
                method: &request.method,
                headers: &request.headers,
                body: &request.body,
            },
            password_hash.as_deref(),
        );
    }

    let ui = ui_path(path);
    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&request.headers) {
            Ok(auth) => auth,
            Err(()) => {
                // A browser navigation to a plugin UI should land on the
                // console login instead of raw JSON. API clients keep the 401.
                if request.method == "GET"
                    && path.starts_with("/plugin-ui/")
                    && request_header(request, "accept")
                        .map(|value| value.contains("text/html"))
                        .unwrap_or(false)
                {
                    return http::response_with_headers(
                        303,
                        "text/plain; charset=utf-8",
                        &[(
                            "Location".to_string(),
                            format!("/?next={}", webui::encode_query_value(path)),
                        )],
                        b"",
                    );
                }
                return error(401, "unauthorized", "authentication required");
            }
        },
        Err(_) => return error(500, "internal", "LAN state unavailable"),
    };
    // `/plugin-ui/{id}/rpc` keeps the same-origin Origin check instead of the
    // management API's custom CSRF header, so a plugin's own page can call it
    // with a plain `fetch` (see `ui_rpc`).
    if auth == AuthKind::Cookie
        && is_write(&request.method)
        && !ui.as_ref().map(|route| route.sub == "rpc").unwrap_or(false)
        && !csrf_ok(request)
    {
        return error(
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        );
    }
    if let Some(ui) = ui {
        // Canonicalise `/plugin-ui/{id}` to `/plugin-ui/{id}/` after auth so
        // relative asset URLs resolve against the plugin root; the session
        // check stays authoritative and only reachable pages redirect.
        if request.method == "GET"
            && ui.slashless
            && package::valid_name(ui.id)
            && plugin::web_assets(ui.id).is_some()
        {
            let location = if ui.token.is_empty() {
                format!("/plugin-ui/{}/", ui.id)
            } else {
                format!("/plugin-ui/{}/{}/", ui.id, ui.token)
            };
            return http::response_with_headers(
                308,
                "text/plain; charset=utf-8",
                &[("Location".to_string(), location)],
                b"",
            );
        }
        return plugin_ui(config, request, &ui, auth, lan);
    }
    route(config, request, lan)
}

/// Serve `/plugin-ui/{id}/{token}/...`: the plugin's package-local web assets,
/// the `/rpc` JSON-RPC bridge, and the SSE/WebSocket realtime streams. A plugin
/// never runs an HTTP listener of its own, and the token must belong to this
/// plugin id.
fn plugin_ui(
    config: &Path,
    request: &HttpRequest,
    ui: &UiPath<'_>,
    auth: AuthKind,
    lan: &Arc<Mutex<Lan>>,
) -> Vec<u8> {
    if !package::valid_name(ui.id) {
        return error(404, "not_found", "unknown plugin UI");
    }
    let Some(assets) = plugin::web_assets(ui.id) else {
        return error(404, "not_found", &format!("plugin {} has no web UI", ui.id));
    };
    if !token_valid(lan, ui.token, ui.id) {
        return error(401, "unauthorized", "invalid or expired plugin token");
    }
    if ui.sub == "rpc" {
        return ui_rpc(config, request, ui.id, auth);
    }
    let base = format!("/plugin-ui/{}/{}/", ui.id, ui.token);
    serve_ui(request, &assets, ui.sub, &base)
}

pub(crate) fn token_valid(lan: &Arc<Mutex<Lan>>, token: &str, plugin: &str) -> bool {
    lan::plugin_token_valid(lan, token, plugin)
}

fn serve_ui(request: &HttpRequest, assets: &plugin::WebAssets, sub: &str, base: &str) -> Vec<u8> {
    if request.method != "GET" {
        return method_not_allowed("GET");
    }
    let Some(relative) = webui::relative_path(sub, &assets.entry) else {
        return error(404, "not_found", "not found");
    };
    if let Some((path, metadata)) = webui::web_file(&assets.root, &relative) {
        return serve_web_file(request, &path, &metadata, base);
    }
    // SPA fallback: only an HTML navigation falls back to the entry. A missing
    // script must stay a 404 so the browser reports it instead of running HTML.
    if wants_html(request) {
        if let Some((path, metadata)) = webui::web_file(&assets.root, &assets.entry) {
            return serve_web_file(request, &path, &metadata, base);
        }
    }
    error(404, "not_found", "not found")
}

fn serve_web_file(
    request: &HttpRequest,
    path: &Path,
    metadata: &fs::Metadata,
    base: &str,
) -> Vec<u8> {
    let mime = http::content_type(&path.to_string_lossy());
    let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
    let etag = webui::etag(modified, metadata.len());
    let headers = webui::web_headers(&etag);
    if webui::not_modified(request_header(request, "if-none-match"), &etag) {
        return http::response_with_headers(304, mime, &headers, b"");
    }
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(cause) => return error(500, "internal", &format!("read UI asset: {}", cause)),
    };
    let body = if mime.starts_with("text/html") {
        webui::inject_base(&data, base)
    } else {
        data
    };
    http::response_with_headers(200, mime, &headers, &body)
}

fn wants_html(request: &HttpRequest) -> bool {
    webui::wants_html(&request.headers)
}

fn ui_rpc(config: &Path, request: &HttpRequest, id: &str, auth: AuthKind) -> Vec<u8> {
    if request.method != "POST" {
        return method_not_allowed("POST");
    }
    if !same_origin(request) {
        return error(403, "forbidden", "cross-origin calls are not allowed");
    }
    if auth == AuthKind::Cookie && request_header(request, "origin").is_none() {
        return error(
            403,
            "forbidden",
            "Origin is required for cookie-authenticated calls",
        );
    }
    let response = match webui::bridge_request(id, &request.body) {
        webui::Bridge::Reject(response) => response,
        webui::Bridge::Forward {
            id: client_id,
            name,
            arguments,
        } => match channel::request(
            config,
            "registry.call",
            json!({ "name": name, "arguments": arguments }),
        ) {
            Ok(mut response) => {
                // The local channel always uses id 1; the page gets its own id.
                response.id = client_id;
                response
            }
            Err(cause) => rpc::err(client_id, -32603, &format!("svcd unreachable: {}", cause)),
        },
    };
    http::response(200, "application/json", rpc::to_line(&response).as_bytes())
}

/// Browsers send `Origin` on cross-origin (and same-origin POST) requests. A
/// mismatched origin is CSRF or DNS rebinding; an absent origin is allowed on
/// the unauthenticated loopback surface but never with a session cookie.
fn same_origin(request: &HttpRequest) -> bool {
    http::same_origin(&request.headers)
}

fn login(request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
    let password = request_header(request, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| {
            serde_json::from_slice::<Value>(&request.body)
                .ok()
                .and_then(|value| {
                    value
                        .get("password")
                        .or_else(|| value.get("token"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        });
    let Some(password) = password else {
        return error(401, "unauthorized", "password required");
    };
    let session = match lan.lock() {
        Ok(mut state) => match state.login(&password) {
            Ok(session) => session,
            Err(message) => return error(401, "unauthorized", &message),
        },
        Err(_) => return error(500, "internal", "auth state unavailable"),
    };
    let body = serde_json::to_vec(&json!({ "ok": true })).unwrap_or_default();
    http::response_with_headers(
        200,
        "application/json",
        &lan::cookie_headers(&session),
        &body,
    )
}

fn logout(request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
    if let Ok(mut state) = lan.lock() {
        state.logout(&request.headers);
    }
    let body = serde_json::to_vec(&json!({ "ok": true })).unwrap_or_default();
    http::response_with_headers(200, "application/json", &lan::clear_cookie_headers(), &body)
}

fn is_write(method: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
}

fn csrf_ok(request: &HttpRequest) -> bool {
    let host = request_header(request, "host").unwrap_or("");
    let origin = request_header(request, "origin").unwrap_or("");
    let csrf = request_header(request, "x-svcd-csrf").unwrap_or("");
    !host.is_empty() && origin == format!("http://{}", host) && csrf == "1"
}

/// Why no complete request head arrived. A timeout or EOF is a closed
/// connection, not a malformed request: answering it with a 400 would be a
/// response the client never asked for and the Unix reactor does not send one.
#[cfg(not(unix))]
enum HeadFailure {
    Closed,
    Response(Vec<u8>),
}

#[cfg(not(unix))]
fn read_head(stream: &mut TcpStream) -> Result<(http::Head, Vec<u8>), HeadFailure> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if buffer.len() > crate::limits::current().max_header {
            return Err(HeadFailure::Response(error(
                400,
                "bad_request",
                "header is too large",
            )));
        }
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(_) => return Err(HeadFailure::Closed),
        };
        if read == 0 {
            return Err(HeadFailure::Closed);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = http::header_end(&buffer) {
            if position > crate::limits::current().max_header {
                return Err(HeadFailure::Response(error(
                    400,
                    "bad_request",
                    "header is too large",
                )));
            }
            let Some((head, body_start)) = http::parse_head(&buffer[..position + 4]) else {
                return Err(HeadFailure::Response(error(
                    400,
                    "bad_request",
                    "malformed request line",
                )));
            };
            return Ok((head, buffer[body_start..].to_vec()));
        }
    }
}

#[cfg(not(unix))]
fn read_body(
    stream: &mut TcpStream,
    head: http::Head,
    initial: &[u8],
) -> Result<HttpRequest, Vec<u8>> {
    let framing =
        http::framing(&head.headers).map_err(|message| error(400, "bad_request", &message))?;
    let limit = body_limit(&head.path);
    if let http::Framing::Length(length) = framing {
        if length > limit {
            return Err(error(413, "payload_too_large", "request body is too large"));
        }
    }
    if !matches!(framing, http::Framing::None)
        && head.version.eq_ignore_ascii_case("HTTP/1.1")
        && http::header(&head.headers, "expect")
            .map(|value| value.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
    {
        stream
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .map_err(|_| error(400, "bad_request", "cannot write response"))?;
    }
    let body = http::collect_body(stream, framing, limit, initial).map_err(|message| {
        let status = if message.contains("too large") {
            413
        } else {
            400
        };
        error(status, code_for(status), &message)
    })?;
    Ok(HttpRequest {
        method: head.method,
        path: head.path,
        version: head.version,
        headers: head.headers,
        body,
    })
}

fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    http::header(&request.headers, name)
}

fn route(config: &std::path::Path, request: &HttpRequest, lan_state: &Arc<Mutex<Lan>>) -> Vec<u8> {
    let (path, query) = request
        .path
        .split_once('?')
        .unwrap_or((request.path.as_str(), ""));
    let mut parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.get(2) == Some(&"files") {
        parts[2] = "file";
    }
    if parts.len() < 3 || parts[0] != "api" || parts[1] != "v1" {
        return error(404, "not_found", "unknown endpoint");
    }

    match (request.method.as_str(), parts.as_slice()) {
        ("GET", ["api", "v1", "compat"]) => {
            ok(serde_json::to_value(crate::compat::detect()).unwrap_or(Value::Null))
        }
        ("GET", ["api", "v1", "auth"]) => match lan_state.lock() {
            Ok(state) => ok(json!({
                "bind": state.bind.to_string(),
                "requestedBind": state.requested_bind.to_string(),
                "authRequired": state.require_auth,
                "passwordSet": state.password_hash().is_some(),
            })),
            Err(_) => error(500, "internal", "auth state unavailable"),
        },
        ("GET", ["api", "v1", "services", id]) => {
            match rpc(config, "svc.status", json!({ "id": decode(id) })) {
                Ok(status) => ok(status),
                Err(response) => response,
            }
        }
        ("POST", ["api", "v1", "services", id, "start"]) => {
            match rpc(config, "svc.start", json!({ "id": decode(id) })) {
                Ok(status) => summary(status),
                Err(response) => response,
            }
        }
        ("POST", ["api", "v1", "services", id, "stop"]) => {
            match rpc(config, "svc.stop", json!({ "id": decode(id) })) {
                Ok(status) => summary(status),
                Err(response) => response,
            }
        }
        ("POST", ["api", "v1", "services", id, "enable"]) => {
            match rpc(config, "svc.enable", json!({ "id": decode(id) })) {
                Ok(status) => summary(status),
                Err(response) => response,
            }
        }
        ("POST", ["api", "v1", "services", id, "disable"]) => {
            match rpc(config, "svc.disable", json!({ "id": decode(id) })) {
                Ok(status) => summary(status),
                Err(response) => response,
            }
        }
        ("POST", ["api", "v1", "plugins", id, "ui-token"]) => {
            plugin_ui_token(&decode(id), lan_state)
        }
        (_, ["api", "v1", "plugins", _, "ui-token"]) => method_not_allowed("POST"),
        ("POST", ["api", "v1", "plugins"]) => install_plugin(config, request, query),
        ("POST", ["api", "v1", "blobs", "uploads"]) => {
            blob_http::create_upload(blob::UploadKind::Blob)
        }
        ("GET", ["api", "v1", "blobs", "uploads", id]) => {
            blob_http::upload_status(blob::UploadKind::Blob, &decode(id))
        }
        ("POST", ["api", "v1", "blobs", "uploads", id, "complete"]) => {
            blob_http::complete_upload(&decode(id), &request.body)
        }
        ("DELETE", ["api", "v1", "blobs", "uploads", id]) => {
            blob_http::abort_upload(blob::UploadKind::Blob, &decode(id))
        }
        (_, ["api", "v1", "blobs"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads", _]) => method_not_allowed("GET, DELETE"),
        (_, ["api", "v1", "blobs", "uploads", _, "complete"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads", _, "parts", _]) => method_not_allowed("PUT"),
        (_, ["api", "v1", "blobs", _]) => method_not_allowed("GET"),
        ("POST", ["api", "v1", "file", "uploads"]) => {
            blob_http::create_upload(blob::UploadKind::File)
        }
        ("GET", ["api", "v1", "file", "uploads", id]) => {
            blob_http::upload_status(blob::UploadKind::File, &decode(id))
        }
        ("POST", ["api", "v1", "file", "uploads", id, "complete"]) => {
            blob_http::file_upload_complete(&decode(id), &request.body)
        }
        ("DELETE", ["api", "v1", "file", "uploads", id]) => {
            blob_http::abort_upload(blob::UploadKind::File, &decode(id))
        }
        (_, ["api", "v1", "file"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "file", "uploads"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "file", "uploads", _]) => method_not_allowed("GET, DELETE"),
        (_, ["api", "v1", "file", "uploads", _, "complete"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "file", "uploads", _, "parts", _]) => method_not_allowed("PUT"),
        (_, ["api", "v1", "file", _]) => method_not_allowed("GET"),
        ("GET", ["api", "v1", "trust"]) => trust_list(),
        ("POST", ["api", "v1", "trust"]) => trust_add(request, query),
        ("DELETE", ["api", "v1", "trust", key]) => trust_remove(&decode(key)),
        (_, ["api", "v1", "trust"]) => method_not_allowed("GET, POST"),
        (_, ["api", "v1", "trust", _]) => method_not_allowed("DELETE"),
        ("DELETE", ["api", "v1", "plugins", id]) => {
            let purge = query_bool(query, "purge").unwrap_or(false);
            match rpc(
                config,
                "plugin.uninstall",
                json!({ "id": decode(id), "purge": purge }),
            ) {
                Ok(_) => ok(json!({ "ok": true })),
                Err(response) => response,
            }
        }
        ("POST", ["api", "v1", "config", id, "reset"]) => reset_config(config, &decode(id)),
        (_, ["api", "v1", "config", _, "reset"]) => method_not_allowed("POST"),
        ("GET", ["api", "v1", "config", id]) => get_config(config, &decode(id)),
        ("PUT", ["api", "v1", "config", id]) => put_config(config, &decode(id), request),
        (_, ["api", "v1", "config", _]) => method_not_allowed("GET, PUT"),
        ("GET", ["api", "v1", "export", id, "config"]) => export_config(config, &decode(id)),
        (_, ["api", "v1", "export", _, "config"]) => method_not_allowed("GET"),
        ("GET", ["api", "v1", "export", id, "spk"]) => export_spk(config, &decode(id)),
        (_, ["api", "v1", "export", _, "spk"]) => method_not_allowed("GET"),
        ("GET", ["api", "v1", "export", id]) => export_spkb(config, &decode(id)),
        (_, ["api", "v1", "export", _]) => method_not_allowed("GET"),
        ("POST", ["api", "v1", "import"]) => import_plugin(request, query),
        (_, ["api", "v1", "import"]) => method_not_allowed("POST"),
        ("GET", ["api", "v1", "logs", id]) => logs(config, &decode(id), query),
        (_, ["api", "v1", "logs", _]) => method_not_allowed("GET"),
        _ => error(404, "not_found", "unknown endpoint"),
    }
}

fn trust_list() -> Vec<u8> {
    match crate::trust::keys() {
        Ok(keys) => ok(json!({
            "keys": keys,
            "directory": paths::trust_dir().to_string_lossy(),
        })),
        Err(cause) => error(500, "internal", &cause),
    }
}

fn trust_add(request: &HttpRequest, query: &str) -> Vec<u8> {
    let Some(key_id) = query_value(query, "key_id") else {
        return error(400, "bad_request", "key_id is required");
    };
    if request.body.is_empty() {
        return error(400, "bad_request", "empty public key");
    }
    match crate::trust::add(&key_id, &request.body) {
        Ok(path) => ok(json!({
            "keyId": key_id,
            "path": path.to_string_lossy(),
        })),
        Err(error) => map_error(&error),
    }
}

fn trust_remove(key_id: &str) -> Vec<u8> {
    match crate::trust::remove(key_id) {
        Ok(true) => ok(json!({ "keyId": key_id, "removed": true })),
        Ok(false) => error(
            404,
            "not_found",
            &format!("unknown trusted key: {}", key_id),
        ),
        Err(error) => map_error(&error),
    }
}

fn plugin_ui_token(id: &str, lan_state: &Arc<Mutex<Lan>>) -> Vec<u8> {
    if !package::valid_name(id) {
        return error(404, "not_found", &format!("unknown plugin UI: {}", id));
    }
    if plugin::web_assets(id).is_none() {
        return error(404, "not_found", &format!("plugin {} has no web UI", id));
    }
    let token = match lan_state.lock() {
        Ok(mut state) => state.issue_plugin_token(id),
        Err(_) => return error(500, "internal", "auth state unavailable"),
    };
    ok(json!({
        "token": token,
        "url": format!("/plugin-ui/{}/{}/", id, token),
        "expiresIn": lan::plugin_token_ttl_secs(),
    }))
}

fn install_plugin(config: &std::path::Path, request: &HttpRequest, query: &str) -> Vec<u8> {
    if request.body.is_empty() {
        return error(400, "bad_request", "empty .spk body");
    }
    let content_type = request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("");
    if !content_type.starts_with("application/octet-stream") {
        return error(
            400,
            "bad_request",
            "Content-Type must be application/octet-stream",
        );
    }
    let strategy = query_value(query, "strategy").unwrap_or_else(|| "replace".into());
    if strategy != "keep-old" && strategy != "replace" {
        return error(400, "bad_request", "strategy must be keep-old or replace");
    }
    let enable = query_bool(query, "enable").unwrap_or(true);
    let allow_hooks = query_bool(query, "allow_hooks").unwrap_or(false);

    let temp_dir = paths::config_dir().join("tmp");
    let _ = fs::create_dir_all(&temp_dir);
    let path = temp_dir.join(format!(
        "upload-{}-{}.spk",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(cause) = fs::write(&path, &request.body) {
        return error(500, "internal", &format!("cannot store upload: {}", cause));
    }
    let result = rpc(
        config,
        "plugin.install",
        json!({
            "path": path.to_string_lossy(),
            "strategy": strategy,
            "allowHooks": allow_hooks,
        }),
    );
    let _ = fs::remove_file(&path);

    match result {
        Ok(status) => {
            if !enable {
                let id = status
                    .get("summary")
                    .and_then(|summary| summary.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let _ = rpc(config, "svc.disable", json!({ "id": id }));
            }
            let summary = status.get("summary").cloned().unwrap_or(Value::Null);
            ok(json!({
                "id": summary.get("id").cloned().unwrap_or(Value::Null),
                "version": summary.get("version").cloned().unwrap_or(Value::Null),
                "state": summary.get("state").cloned().unwrap_or(Value::Null),
            }))
        }
        Err(response) => response,
    }
}

pub(crate) fn require_plugin(config: &Path, id: &str) -> Result<(), Vec<u8>> {
    if !package::valid_name(id) {
        return Err(error(404, "not_found", &format!("unknown plugin: {}", id)));
    }
    let status = rpc(config, "svc.status", json!({ "id": id }))?;
    let kind = status
        .pointer("/summary/kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "plugin" {
        Ok(())
    } else {
        Err(error(404, "not_found", &format!("not a plugin: {}", id)))
    }
}

fn config_lock() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn get_config(config: &Path, id: &str) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    let Some(path) = plugin::active_config(id) else {
        return error(404, "not_found", &format!("plugin {} has no config", id));
    };
    match fs::read(&path) {
        Ok(data) => http::response(200, "text/plain", &data),
        Err(cause) => error(500, "internal", &format!("read config: {}", cause)),
    }
}

fn put_config(config: &Path, id: &str, request: &HttpRequest) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    let _guard = config_lock();
    let _lock = match plugin::lock_config(id) {
        Ok(lock) => lock,
        Err(message) => return error(500, "internal", &message),
    };
    match plugin::replace_config(id, &request.body, true) {
        Ok(()) => ok(json!({ "ok": true })),
        Err(message) => config_error(&message),
    }
}

fn reset_config(config: &Path, id: &str) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    let Some((template, _)) = plugin::config_template(id) else {
        return error(
            404,
            "not_found",
            &format!("plugin {} has no config template", id),
        );
    };
    let _guard = config_lock();
    let _lock = match plugin::lock_config(id) {
        Ok(lock) => lock,
        Err(message) => return error(500, "internal", &message),
    };
    match plugin::replace_config(id, &template, true) {
        Ok(()) => ok(json!({ "ok": true })),
        Err(message) => config_error(&message),
    }
}

fn export_spkb(config: &Path, id: &str) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    match plugin::export_spkb(id) {
        Ok((version, data)) => download(&format!("{}-{}.spkb", id, version), &data),
        Err(message) => error(500, "internal", &message),
    }
}

/// The installable, signed package reconstructed from the current version.
fn export_spk(config: &Path, id: &str) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    match plugin::export_spk(id) {
        Ok((version, data)) => download(&format!("{}-{}.spk", id, version), &data),
        Err(message) => error(500, "internal", &message),
    }
}

fn export_config(config: &Path, id: &str) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    match plugin::export_config(id) {
        Ok((version, data)) => download(&format!("{}-{}.config", id, version), &data),
        Err(message) if message.contains("has no config") => error(404, "not_found", &message),
        Err(message) => error(500, "internal", &message),
    }
}

fn import_plugin(request: &HttpRequest, query: &str) -> Vec<u8> {
    let config_id = query_value(query, "config_id");
    let no_config = query_bool(query, "no_config").unwrap_or(false);
    let force = query_bool(query, "force").unwrap_or(false);
    let _guard = config_lock();
    // The HTTP surface is already authorised by the password / loopback
    // policy; do not re-check the daemon's OS privilege here (the CLI does).
    match transfer::import(&request.body, config_id.as_deref(), no_config, force, false) {
        Ok(outcome) => {
            let mut body = json!({
                "ok": true,
                "configApplied": outcome.config.is_some(),
            });
            if let Some(status) = outcome.status {
                body["id"] = json!(status.summary.id);
                body["version"] = json!(status.summary.version);
                body["state"] = json!(status.summary.state);
            } else if let Some(id) = outcome.config.clone() {
                body["id"] = json!(id);
            }
            if !outcome.warnings.is_empty() {
                body["warnings"] = json!(outcome.warnings);
            }
            ok(body)
        }
        Err(transfer::Error::Usage(message)) => error(400, "bad_request", &message),
        Err(transfer::Error::Failed(message)) => import_error(&message),
    }
}

fn logs(config: &Path, id: &str, query: &str) -> Vec<u8> {
    if let Err(response) = require_plugin(config, id) {
        return response;
    }
    let lines = match query_value(query, "lines") {
        None => 200,
        Some(value) => match value.parse::<usize>() {
            Ok(lines) if lines <= 2000 => lines,
            _ => {
                return error(
                    400,
                    "bad_request",
                    "lines must be an integer between 0 and 2000",
                )
            }
        },
    };
    let path = paths::logs_dir().join(format!("{}.log", id));
    match log::tail(&path, lines) {
        Ok(text) => http::response(200, "text/plain; charset=utf-8", cap_log(text).as_bytes()),
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
            error(404, "not_found", &format!("no log for {}", id))
        }
        Err(cause) => error(500, "internal", &format!("cannot read log: {}", cause)),
    }
}

fn ui_page() -> Vec<u8> {
    http::response_with_headers(
        200,
        "text/html; charset=utf-8",
        &[
            ("Cache-Control".to_string(), "no-store".to_string()),
            ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
        ],
        UI.as_bytes(),
    )
}

fn cap_log(mut text: String) -> String {
    if text.len() <= crate::limits::current().log_max_bytes as usize {
        return text;
    }
    let mut start = text.len() - crate::limits::current().log_max_bytes as usize;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    if let Some(newline) = text[start..].find('\n') {
        // Keep complete lines when possible; a single line longer than the
        // budget keeps its tail rather than becoming empty.
        let candidate = start + newline + 1;
        if candidate < text.len() {
            start = candidate;
        }
    }
    text.drain(..start);
    text
}

fn method_not_allowed(allow: &str) -> Vec<u8> {
    http::method_not_allowed(allow)
}

fn download(filename: &str, data: &[u8]) -> Vec<u8> {
    http::response_with_headers(
        200,
        "application/octet-stream",
        &[(
            "Content-Disposition".to_string(),
            format!("attachment; filename=\"{}\"", filename),
        )],
        data,
    )
}

fn import_error(message: &str) -> Vec<u8> {
    let lower = message.to_ascii_lowercase();
    let status = if lower.contains("core service") || lower.contains("unknown plugin") {
        404
    } else if lower.contains("root / administrator") {
        403
    } else if lower.contains("already installed")
        || lower.contains("conflict")
        || lower.contains("configcheck")
    {
        409
    } else if lower.contains("exceeds 1 mib") {
        413
    } else if lower.contains("signature")
        || lower.contains("platform")
        || lower.contains("protocol")
        || lower.contains("unsafe")
        || lower.contains("setuid")
        || lower.contains("hooks")
        || lower.contains("invalid")
        || lower.contains("not declared")
        || lower.contains("missing")
        || lower.contains("does not match")
        || lower.contains("unsupported")
    {
        400
    } else {
        500
    };
    error(status, code_for(status), message)
}

fn config_error(message: &str) -> Vec<u8> {
    let lower = message.to_ascii_lowercase();
    let status = if lower.contains("unknown plugin") {
        404
    } else if lower.contains("exceeds 1 mib") {
        413
    } else if lower.contains("configcheck") {
        409
    } else {
        500
    };
    error(status, code_for(status), message)
}

/// A daemon RPC failure: status, error code and message. The caller renders
/// the wire envelope; the face uses the same mapping as the legacy router.
pub(crate) type RpcFailure = (u16, &'static str, String);

pub(crate) fn rpc_result(
    config: &std::path::Path,
    method: &str,
    params: Value,
) -> Result<Value, RpcFailure> {
    let response = channel::request(config, method, params)
        .map_err(|cause| (500, "internal", format!("svcd unreachable: {}", cause)))?;
    if let Some(error) = response.error {
        let status = status_for_error(&error.message);
        return Err((status, code_for(status), error.message));
    }
    response
        .result
        .ok_or_else(|| (500, "internal", "empty daemon response".to_string()))
}

pub(crate) fn rpc(config: &std::path::Path, method: &str, params: Value) -> Result<Value, Vec<u8>> {
    rpc_result(config, method, params)
        .map_err(|(status, code, message)| error(status, code, &message))
}

pub(crate) fn map_error(message: &str) -> Vec<u8> {
    let status = status_for_error(message);
    error(status, code_for(status), message)
}

fn status_for_error(message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        504
    } else if lower.contains("exceeds") || lower.contains("too large") {
        413
    } else if lower.contains("unknown keyid") || lower.contains("trust root") {
        400
    } else if lower.contains("id is a core service") {
        404
    } else if lower.contains("unknown") || lower.contains("not found") {
        404
    } else if lower.contains("configcheck") {
        409
    } else if lower.contains("not a plugin")
        || lower.contains("core service")
        || lower.contains("conflict")
        || lower.contains("cannot be enabled")
    {
        409
    } else if lower.contains("signature")
        || lower.contains("platform")
        || lower.contains("protocol")
        || lower.contains("unsafe")
        || lower.contains("setuid")
        || lower.contains("hooks")
        || lower.contains("invalid")
        || lower.contains("not declared")
        || lower.contains("missing")
        || lower.contains("public key")
    {
        400
    } else {
        500
    }
}

fn code_for(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        413 => "payload_too_large",
        504 => "timeout",
        _ => "internal",
    }
}

fn summary(status: Value) -> Vec<u8> {
    ok(json!({
        "summary": status.get("summary").cloned().unwrap_or(Value::Null)
    }))
}

fn ok(value: Value) -> Vec<u8> {
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&value).unwrap_or_default(),
    )
}

fn error(status: u16, code: &str, message: &str) -> Vec<u8> {
    http::json_error(status, code, message)
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            Some(decode(value))
        } else {
            None
        }
    })
}

fn query_bool(query: &str, key: &str) -> Option<bool> {
    query_value(query, key).map(|value| value == "true" || value == "1")
}

fn decode(value: &str) -> String {
    http::percent_decode(value, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_precedence_is_env_then_config_then_default() {
        let a: SocketAddr = "127.0.0.1:1234".parse().expect("addr");
        let b: SocketAddr = "0.0.0.0:5678".parse().expect("addr");
        assert_eq!(resolve_bind(Some("127.0.0.1:1234"), Some(b)), a);
        assert_eq!(resolve_bind(Some("not-an-addr"), Some(b)), b);
        assert_eq!(resolve_bind(None, Some(b)), b);
        assert_eq!(resolve_bind(None, None), default_bind());
    }

    #[cfg(unix)]
    #[test]
    fn the_legacy_bridge_queue_is_bounded() {
        assert_eq!(legacy::LEGACY_QUEUE_LIMIT, 64);
        let queue = JobQueue {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            limit: legacy::LEGACY_QUEUE_LIMIT,
        };
        let job = || {
            Work::Http(HttpJob {
                connection: 1,
                request: HttpRequest {
                    method: "GET".into(),
                    path: "/api/v1/version".into(),
                    version: "HTTP/1.1".into(),
                    headers: Vec::new(),
                    body: Vec::new(),
                },
                keep_alive: false,
            })
        };
        for _ in 0..legacy::LEGACY_QUEUE_LIMIT {
            assert!(queue.try_push(job()).is_ok());
        }
        assert!(
            queue.try_push(job()).is_err(),
            "a full bridge rejects the job instead of growing the queue"
        );
        assert_eq!(
            queue.queue.lock().expect("queue").len(),
            legacy::LEGACY_QUEUE_LIMIT,
            "the rejected job must not be queued"
        );
    }

    /// The Windows fallback ties one admitted connection to one thread, so the
    /// admission lease is the thread cap. Prove both caps reject without
    /// consuming a slot and that dropping a lease frees the global and per-IP
    /// counts.
    #[test]
    fn admission_enforces_global_and_per_ip_caps_and_releases() {
        let admission = Arc::new(Mutex::new(Admission::new(2, 1)));

        let first = admit(&admission, "10.0.0.1");
        assert!(first.is_some(), "first connection is admitted");
        assert!(
            admit(&admission, "10.0.0.1").is_none(),
            "the second connection from one IP hits max_connections_per_ip"
        );
        let second = admit(&admission, "10.0.0.2");
        assert!(second.is_some(), "a different IP still fits the global cap");
        assert!(
            admit(&admission, "10.0.0.3").is_none(),
            "a third connection hits max_connections"
        );
        assert_eq!(active(&admission), 2, "rejections do not consume slots");

        drop(first);
        assert_eq!(active(&admission), 1, "the lease drop frees one slot");
        assert!(
            admit(&admission, "10.0.0.1").is_some(),
            "the per-IP slot is reusable after the lease is dropped"
        );
    }

    fn admit(admission: &Arc<Mutex<Admission>>, peer: &str) -> Option<ConnectionLease> {
        ConnectionLease::acquire(admission, peer.parse().expect("peer ip"))
    }

    fn active(admission: &Arc<Mutex<Admission>>) -> usize {
        Admission::lock(admission).active
    }
}
