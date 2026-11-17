//! Runtime assembly, authentication, and the transfer/MCP paths that are not
//! yet faces.
//!
//! `bind` in `svcd.conf` chooses the listener address. A non-loopback bind
//! requires `auth_passwd`; without it the daemon falls back to loopback so the
//! operator can still run `svc auth passwd`. Management JSON projections and
//! the stream endpoints live in `face/` (#117–#121); the Unix server loop owns
//! connections and control transport (#124/#125); this module is the
//! composition root plus the Windows per-connection fallback.

#[cfg(any(not(unix), test))]
use std::collections::HashMap;
use std::fs;
#[cfg(not(unix))]
use std::io::{Read, Write};
#[cfg(not(unix))]
use std::net::TcpStream;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(not(unix))]
use std::thread;
#[cfg(not(unix))]
use std::time::Duration;

use serde_json::{json, Value};

use crate::blob;
use crate::blob_http;
use crate::channel;
use crate::face::plugin_ui::{ui_path, UiPath};
use crate::http;
use crate::lan::{self, AuthKind, Lan, LanStatus};
use crate::log;
use crate::mcp_http;
use crate::package;
use crate::plugin;
use crate::rpc;
use crate::server::http::engine::HttpRequest;
#[cfg(not(unix))]
use crate::server::http::engine::{
    self, keep_alive_response, wants_keep_alive, wants_keep_alive_head,
};
use crate::settings;
#[cfg(not(unix))]
use crate::threads;
use crate::webui;

pub struct Api {
    lan: Arc<Mutex<Lan>>,
    /// Stops the server loop and drains its control connections on shutdown.
    #[cfg(unix)]
    loop_handle: crate::server::control::handle::LoopHandle,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}

fn resolve_bind(env: Option<&str>, configured: Option<SocketAddr>) -> SocketAddr {
    env.and_then(|value| value.parse::<SocketAddr>().ok())
        .or(configured)
        .unwrap_or_else(default_bind)
}

#[cfg(unix)]
pub fn spawn(
    config: PathBuf,
    raw_hub: Arc<crate::rawhub::RawHub>,
    control: std::os::unix::net::UnixListener,
    pending: mpsc::SyncSender<crate::channel::PendingCall>,
    wake: Arc<crate::wake::Wake>,
) -> std::io::Result<Arc<Api>> {
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
    // The control channel is the server's lifeline now, so an HTTP bind
    // failure disables only HTTP; the loop still serves the CLI.
    let listener = match TcpListener::bind(bind) {
        Ok(listener) => match listener.local_addr() {
            Ok(local) => {
                state.bind = local;
                let port_file = config.join("http.port");
                if let Err(error) = fs::write(&port_file, format!("127.0.0.1:{}", local.port())) {
                    log::svcd(&format!(
                        "svcd: cannot write {}: {}",
                        port_file.display(),
                        error
                    ));
                }
                log::svcd(&format!(
                    "svcd: HTTP API on {} (auth required: {})",
                    local, require_auth
                ));
                Some(listener)
            }
            Err(error) => {
                log::svcd(&format!("svcd: HTTP API disabled: {}", error));
                None
            }
        },
        Err(error) => {
            log::svcd(&format!("svcd: HTTP API disabled: {}", error));
            None
        }
    };

    let lan = Arc::new(Mutex::new(state));
    let fallback_config = config.clone();
    let face: Arc<dyn crate::server::http::Face> =
        Arc::new(crate::face::api::ApiFace::new(config.clone(), lan.clone()));
    let routes: Arc<dyn crate::server::http::server::PreBodyRoutes> = Arc::new(PreBodyFaces);
    let fallback: crate::server::http::server::FallbackHandler =
        Arc::new(move |request, lan| respond(&fallback_config, request, lan));
    let loop_handle = crate::server::http::server::start(
        listener,
        control,
        pending,
        wake,
        lan.clone(),
        config,
        raw_hub,
        face,
        routes,
        fallback,
    )?;
    Ok(Arc::new(Api { lan, loop_handle }))
}

#[cfg(not(unix))]
pub fn spawn(config: PathBuf, raw_hub: Arc<crate::rawhub::RawHub>) -> std::io::Result<Arc<Api>> {
    let configured = settings::current().ok().and_then(|settings| settings.bind);
    let requested = resolve_bind(std::env::var("SVCD_BIND").ok().as_deref(), configured);
    let password_hash = settings::current()
        .ok()
        .and_then(|settings| settings.auth_passwd.clone());

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
    let _ = raw_hub;
    thread::Builder::new()
        .name("svcd-http".into())
        .stack_size(threads::io_stack())
        .spawn(move || listener_loop(Some(listener), lan, config))?;
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

    /// Stop the server loop after the supervisor loop returned. On Unix this
    /// drains queued control replies and flushes them before the process exits.
    pub fn shutdown(&self) {
        #[cfg(unix)]
        self.loop_handle.shutdown();
    }
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
    if crate::server::http::fallback::drive(stream, handler, &[]).is_err() {
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

/// The composition root's pre-body stream faces. The server loop holds this
/// trait object instead of depending on the face layer directly.
#[cfg(unix)]
struct PreBodyFaces;

#[cfg(unix)]
impl crate::server::http::server::PreBodyRoutes for PreBodyFaces {
    fn plugin_ui(
        &self,
        head: &http::Head,
        peer: IpAddr,
        lan: &Arc<Mutex<Lan>>,
        config: &Path,
        raw_hub: Arc<crate::rawhub::RawHub>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        crate::face::plugin_ui::PluginUiFace::route_for(&head.method, &head.path)?;
        Some(crate::face::plugin_ui::PluginUiFace::exchange(
            head,
            peer,
            lan,
            config.to_path_buf(),
            Some(raw_hub),
        ))
    }

    fn package_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
        config: &Path,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        crate::face::install::target(head)?;
        Some(crate::face::install::exchange(
            head,
            lan,
            config.to_path_buf(),
        ))
    }

    fn blob_upload(
        &self,
        head: &http::Head,
        lan: &Arc<Mutex<Lan>>,
    ) -> Option<Result<crate::server::http::Exchange, Vec<u8>>> {
        crate::face::blob_upload::upload_target(head)?;
        Some(crate::face::blob_upload::exchange(head, lan))
    }

    fn body_policy(&self, head: &http::Head) -> crate::server::http::BodyPolicy {
        crate::api::server_body_policy(head)
    }
}

/// Run one package install/import through the face and the Windows fallback
/// driver. The fallback owns the socket and runs staging jobs on this
/// connection thread, so a slow client only blocks its own connection.
#[cfg(not(unix))]
fn serve_package_upload(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
    config: PathBuf,
) {
    use crate::server::http::Exchange;

    match crate::face::install::exchange(head, lan, config) {
        Err(response) => {
            let _ = stream.write_all(&response);
        }
        Ok(Exchange::Response(_)) => {
            // A streamed route must not answer with a memory response; the
            // Unix loop renders the same 500 before any handler bytes go out.
            let _ = stream.write_all(&http::json_error(
                500,
                "internal",
                "a streamed package upload route returned a memory response",
            ));
        }
        Ok(Exchange::Stream(handler)) => {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(crate::limits::current().transfer_timeout));
            let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
            if crate::server::http::fallback::drive(stream, handler, initial).is_err() {
                let _ = stream.write_all(&crate::server::http::stream_busy_response());
            }
        }
    }
}

/// Run one blob/file upload through the face and the Windows fallback driver.
/// The fallback owns the socket and runs upload staging jobs on this
/// connection thread, so a slow client only blocks its own connection.
#[cfg(not(unix))]
fn serve_blob_upload(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
) {
    use crate::server::http::Exchange;

    match crate::face::blob_upload::exchange(head, lan) {
        Err(response) => {
            let _ = stream.write_all(&response);
        }
        Ok(Exchange::Response(response)) => {
            let _ = stream.write_all(&response.into_bytes());
        }
        Ok(Exchange::Stream(handler)) => {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(crate::limits::current().transfer_timeout));
            let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
            if crate::server::http::fallback::drive(stream, handler, initial).is_err() {
                let _ = stream.write_all(&crate::server::http::stream_busy_response());
            }
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
    // stream handlers below install their own transfer timeout where a
    // longer transfer is expected.
    let _ = stream.set_read_timeout(Some(idle_timeout));
    let _ = stream.set_write_timeout(Some(idle_timeout));
    let face = crate::face::api::ApiFace::new(config.clone(), lan.clone());
    loop {
        let (head, initial) = match read_head(&mut stream) {
            Ok(head) => head,
            Err(HeadFailure::Closed) => return,
            Err(HeadFailure::Response(response)) => {
                let _ = stream.write_all(&response);
                return;
            }
        };
        // Resolve the face route and the principal exactly once, before any
        // body byte is read. The route's BodyPolicy is the only body limit.
        let planned = match crate::server::http::route_face_request(&face, &head, peer, &lan) {
            Ok(planned) => planned,
            Err(response) => {
                let _ = stream.write_all(&response);
                return;
            }
        };
        if let Some(prepared) = planned {
            let keep_alive = wants_keep_alive_head(&head);
            let body = match read_body(&mut stream, &head, &initial, prepared.route.body) {
                Ok(body) => body,
                Err(response) => {
                    let _ = stream.write_all(&response);
                    return;
                }
            };
            let prepared = match crate::server::http::attach_face_body(prepared, &body) {
                Ok(prepared) => prepared,
                Err(response) => {
                    let _ = stream.write_all(&response);
                    return;
                }
            };
            let result = match prepared.route.dispatch {
                crate::server::http::Dispatch::Inline => {
                    crate::server::http::dispatch_inline(&face, &prepared.route, prepared.request)
                }
                crate::server::http::Dispatch::Blocking => {
                    crate::server::http::dispatch_blocking_now(
                        &face,
                        &prepared.route,
                        prepared.request,
                    )
                }
            };
            match result {
                Ok(crate::server::http::Exchange::Stream(handler)) => {
                    // A streamed response owns this connection until it closes.
                    // The fallback writes the shared 503 if the stream budget is
                    // full before any handler bytes go out.
                    drive_fallback(&mut stream, handler);
                    return;
                }
                result => {
                    let mut response = crate::server::http::render_exchange(result);
                    if keep_alive {
                        response = keep_alive_response(response);
                    }
                    if stream.write_all(&response).is_err() {
                        return;
                    }
                    if !keep_alive {
                        return;
                    }
                    continue;
                }
            }
        }
        // The face leaves these pre-body stream routes to their own faces.
        if crate::face::plugin_ui::PluginUiFace::route_for(&head.method, &head.path).is_some() {
            serve_plugin_ui(&mut stream, &head, peer, &lan, config.clone());
            return;
        }
        if crate::face::install::target(&head).is_some() {
            serve_package_upload(&mut stream, &head, &initial, &lan, config.clone());
            return;
        }
        if crate::face::blob_upload::upload_target(&head).is_some() {
            serve_blob_upload(&mut stream, &head, &initial, &lan);
            return;
        }
        let body_policy = server_body_policy(&head);
        let body = match read_body(&mut stream, &head, &initial, body_policy) {
            Ok(body) => body,
            Err(response) => {
                let _ = stream.write_all(&response);
                return;
            }
        };
        let request = HttpRequest {
            method: head.method,
            path: head.path,
            version: head.version,
            headers: head.headers,
            body,
        };
        let keep_alive = wants_keep_alive(&request);
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
        Ok(response) => crate::server::mcp::result::finish_modern_tool_call(call.id, response),
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

/// Serve the paths that still own their protocol: MCP, plugin UI rpc/static
/// assets, and the transfer fallbacks the face deliberately leaves alone.
fn respond(config: &Path, request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
    let path = request
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(request.path.as_str());
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
    route(config, request)
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

pub(crate) fn is_write(method: &str) -> bool {
    http::is_write_method(method)
}

fn csrf_ok(request: &HttpRequest) -> bool {
    csrf_ok_headers(&request.headers)
}

pub(crate) fn csrf_ok_headers(headers: &[(String, String)]) -> bool {
    http::csrf_ok(headers)
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
    head: &http::Head,
    initial: &[u8],
    body: crate::server::http::BodyPolicy,
) -> Result<Vec<u8>, Vec<u8>> {
    let framing =
        http::framing(&head.headers).map_err(|message| error(400, "bad_request", &message))?;
    let limit = body.limit();
    if !body.accepts(framing) {
        return Err(error(413, "payload_too_large", "request body is too large"));
    }
    if body.confirms_body(head, framing) {
        stream
            .write_all(http::CONTINUE_RESPONSE)
            .map_err(|_| error(400, "bad_request", "cannot write response"))?;
    }
    http::collect_body(stream, framing, limit, initial).map_err(|message| {
        let status = if message.contains("too large") {
            413
        } else {
            400
        };
        error(status, code_for(status), &message)
    })
}

fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    http::header(&request.headers, name)
}

/// Body policy for the routes the face leaves to the server-owned fallback
/// (the /mcp body cap and the blob/file session metadata). These legacy routes
/// still live in this composition root, so this is the last place a path
/// chooses a body limit; their transport/bearer auth stays where it was
/// (`respond` / `mcp_http`) until they become faces. A new endpoint declares
/// its policy in `Face::route` and never touches the HTTP engine.
pub(crate) fn server_body_policy(head: &http::Head) -> crate::server::http::BodyPolicy {
    use crate::server::http::BodyPolicy;

    let limits = crate::limits::current();
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(&head.path);
    if path.trim_end_matches('/').ends_with("/complete")
        && (path.starts_with("/api/v1/blobs/uploads/")
            || path.starts_with("/api/v1/file/uploads/")
            || path.starts_with("/api/v1/files/uploads/"))
    {
        BodyPolicy::Buffered {
            limit: limits.max_complete_body,
        }
    } else {
        BodyPolicy::Buffered {
            limit: limits.max_json_body,
        }
    }
}

/// The buffered routes served by the fallback handler are the blob/file
/// session routes (#118/#119); every management route and every streamed
/// transfer is claimed before this function runs.
fn route(_config: &std::path::Path, request: &HttpRequest) -> Vec<u8> {
    let (path, _query) = request
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
        _ => error(404, "not_found", "unknown endpoint"),
    }
}

fn method_not_allowed(allow: &str) -> Vec<u8> {
    http::method_not_allowed(allow)
}

pub(crate) fn require_plugin_result(
    config: &Path,
    id: &str,
) -> Result<(), (u16, &'static str, String)> {
    if !package::valid_name(id) {
        return Err((404, "not_found", format!("unknown plugin: {}", id)));
    }
    let status = rpc_result(config, "svc.status", json!({ "id": id }))?;
    let kind = status
        .pointer("/summary/kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "plugin" {
        Ok(())
    } else {
        Err((404, "not_found", format!("not a plugin: {}", id)))
    }
}

pub(crate) fn config_error_status(message: &str) -> (u16, &'static str, String) {
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
    (status, code_for(status), message.to_string())
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
    let (status, code, message) = map_error_status(message);
    error(status, code, &message)
}

pub(crate) fn map_error_status(message: &str) -> (u16, &'static str, String) {
    let status = status_for_error(message);
    (status, code_for(status), message.to_string())
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


fn error(status: u16, code: &str, message: &str) -> Vec<u8> {
    http::json_error(status, code, message)
}

pub(crate) fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            Some(decode(value))
        } else {
            None
        }
    })
}

pub(crate) fn query_bool(query: &str, key: &str) -> Option<bool> {
    query_value(query, key).map(|value| value == "true" || value == "1")
}

fn decode(value: &str) -> String {
    http::percent_decode(value, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::http::route_face_request;

    #[test]
    fn a_public_face_route_skips_transport_auth() {
        use crate::server::http::Face;

        let bind = "127.0.0.1:0".parse().expect("bind");
        let lan = Arc::new(Mutex::new(Lan::new(bind, bind, true, Some("hash".into()))));
        let face = crate::face::api::ApiFace::new(".", lan.clone());
        let peer = "127.0.0.1".parse().expect("peer");

        assert!(face.route("POST", "/login").expect("route").public);
        let login = http::Head {
            method: "POST".into(),
            path: "/login".into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
        };
        let prepared = route_face_request(&face, &login, peer, &lan)
            .expect("a public route must not fail authentication")
            .expect("the face claims login");
        assert_eq!(prepared.request.principal, AuthKind::None);

        let protected = http::Head {
            method: "GET".into(),
            path: "/api/v1/auth".into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
        };
        assert!(
            route_face_request(&face, &protected, peer, &lan).is_err(),
            "a protected route still requires a principal"
        );
    }

    #[test]
    fn bind_precedence_is_env_then_config_then_default() {
        let a: SocketAddr = "127.0.0.1:1234".parse().expect("addr");
        let b: SocketAddr = "0.0.0.0:5678".parse().expect("addr");
        assert_eq!(resolve_bind(Some("127.0.0.1:1234"), Some(b)), a);
        assert_eq!(resolve_bind(Some("not-an-addr"), Some(b)), b);
        assert_eq!(resolve_bind(None, Some(b)), b);
        assert_eq!(resolve_bind(None, None), default_bind());
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
