//! Minimal `/api/v1` HTTP management API and LAN authentication.
//!
//! The listener starts on loopback; `svc lan enable` can rebind it to a LAN
//! address and turns on bearer/cookie auth. Every request is translated into the
//! same daemon RPC the CLI uses, so the HTTP surface never owns a second copy of
//! service state.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::channel;
use crate::http;
use crate::lan::{self, AuthKind, Lan, LanStatus};
use crate::log;
use crate::mcp_http;
use crate::package;
use crate::paths;
use crate::plugin;
use crate::rpc;
use crate::threads;
use crate::transfer;
use crate::uichannel;
use crate::webui;

const DEFAULT_PORT: u16 = 8787;
const MAX_BODY: u64 = 512 * 1024 * 1024;
const MAX_JSON_BODY: u64 = 1024 * 1024;
const MAX_LOG_BODY: usize = 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;
const UI: &str = include_str!("web/index.html");

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Serialise config writes inside this process. The on-disk `config.lock`
/// covers other processes on Unix; this mutex also covers Windows, where
/// `flock` is a no-op, and keeps two HTTP threads from sharing the same
/// `config.tmp.<pid>` path.
static CONFIG_WRITES: Mutex<()> = Mutex::new(());

pub struct Api {
    config: PathBuf,
    port: u16,
    lan: Arc<Mutex<Lan>>,
    commands: Sender<Command>,
    current: Arc<Mutex<SocketAddr>>,
}

enum Command {
    Rebind {
        addr: SocketAddr,
        reply: Sender<Result<(), String>>,
    },
}

pub fn spawn(config: PathBuf) -> std::io::Result<Arc<Api>> {
    let port = std::env::var("SVCD_HTTP_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let state = Lan::load(&config);
    let bind = if state.enabled {
        state.bind
    } else {
        "127.0.0.1".parse().expect("loopback")
    };
    let listener = TcpListener::bind((bind, port))?;
    let local = listener.local_addr()?;
    let port_file = config.join("http.port");
    fs::write(&port_file, format!("127.0.0.1:{}", local.port())).map_err(|error| {
        std::io::Error::other(format!("write {}: {}", port_file.display(), error))
    })?;
    log::svcd(&format!(
        "svcd: HTTP API on {}:{}",
        local.ip(),
        local.port()
    ));

    let lan = Arc::new(Mutex::new(state));
    let (commands, receiver) = mpsc::channel();
    let current = Arc::new(Mutex::new(local));
    let api = Arc::new(Api {
        config: config.clone(),
        port: local.port(),
        lan: lan.clone(),
        commands,
        current: current.clone(),
    });
    thread::Builder::new()
        .name("svcd-http".into())
        .stack_size(threads::IO_STACK)
        .spawn(move || listener_loop(Some(listener), local, receiver, lan, config, current))?;
    Ok(api)
}

impl Api {
    pub fn enable(&self, bind: IpAddr) -> Result<(LanStatus, Option<String>), String> {
        let mut state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        let previous = (state.enabled, state.bind);
        let token = state.enable(bind);
        if bind != previous.1 {
            if let Err(error) = self.rebind(SocketAddr::new(bind, self.port)) {
                state.enabled = previous.0;
                state.bind = previous.1;
                return Err(error);
            }
        }
        if let Err(error) = state.save(&self.config) {
            state.enabled = previous.0;
            state.bind = previous.1;
            return Err(error);
        }
        Ok((state.status(), token))
    }

    pub fn disable(&self) -> Result<LanStatus, String> {
        let mut state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        let previous = (state.enabled, state.bind);
        state.disable();
        if let Err(error) = self.rebind(SocketAddr::new(state.bind, self.port)) {
            state.enabled = previous.0;
            state.bind = previous.1;
            return Err(error);
        }
        if let Err(error) = state.save(&self.config) {
            state.enabled = previous.0;
            state.bind = previous.1;
            return Err(error);
        }
        Ok(state.status())
    }

    pub fn rotate(&self) -> Result<String, String> {
        let mut state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        let token = state.rotate();
        state.save(&self.config)?;
        Ok(token)
    }

    pub fn status(&self) -> Result<LanStatus, String> {
        let state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        Ok(state.status())
    }

    fn rebind(&self, addr: SocketAddr) -> Result<(), String> {
        let current = *self
            .current
            .lock()
            .map_err(|_| "HTTP listener state poisoned")?;
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(Command::Rebind { addr, reply })
            .map_err(|_| "HTTP listener is not running".to_string())?;
        // A blocking accept needs a connection to return from accept so the
        // loop can observe the queued command and rebind. The loop drops this
        // probe before dispatching a normal request.
        wake_listener(current);
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "HTTP rebind timed out".to_string())?
    }
}

fn wake_listener(current: SocketAddr) {
    let target = if current.ip().is_unspecified() {
        match current {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), current.port()),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), current.port()),
        }
    } else {
        current
    };
    let _ = TcpStream::connect_timeout(&target, Duration::from_millis(500));
}

fn listener_loop(
    mut listener: Option<TcpListener>,
    mut addr: SocketAddr,
    receiver: Receiver<Command>,
    lan: Arc<Mutex<Lan>>,
    config: PathBuf,
    current: Arc<Mutex<SocketAddr>>,
) {
    loop {
        // Handle commands before blocking in accept. Commands that arrive
        // while blocked are woken by a loopback probe from Api::rebind.
        while let Ok(Command::Rebind {
            addr: new_addr,
            reply,
        }) = receiver.try_recv()
        {
            apply_rebind(&mut listener, &mut addr, new_addr, reply, &current);
        }
        let Some(active) = listener.as_ref() else {
            match receiver.recv() {
                Ok(Command::Rebind {
                    addr: new_addr,
                    reply,
                }) => {
                    apply_rebind(&mut listener, &mut addr, new_addr, reply, &current);
                }
                Err(_) => return,
            }
            continue;
        };
        let accepted = active.accept();
        match accepted {
            Ok((stream, _)) => {
                // The accepted socket may be the probe that woke us for a
                // pending rebind. Commands are queued before the probe is
                // connected, so try_recv sees them here.
                let mut pending = Vec::new();
                while let Ok(command) = receiver.try_recv() {
                    pending.push(command);
                }
                if !pending.is_empty() {
                    drop(stream);
                    for Command::Rebind {
                        addr: new_addr,
                        reply,
                    } in pending
                    {
                        apply_rebind(&mut listener, &mut addr, new_addr, reply, &current);
                    }
                    continue;
                }
                let lan = lan.clone();
                let config = config.clone();
                let _ = thread::Builder::new()
                    .name("svcd-http-conn".into())
                    .stack_size(threads::REQUEST_STACK)
                    .spawn(move || handle(stream, config, lan));
            }
            Err(error) => {
                log::svcd(&format!("svcd: HTTP accept failed: {}", error));
                listener = None;
            }
        }
    }
}

fn apply_rebind(
    listener: &mut Option<TcpListener>,
    addr: &mut SocketAddr,
    new_addr: SocketAddr,
    reply: Sender<Result<(), String>>,
    current: &Arc<Mutex<SocketAddr>>,
) {
    *listener = None;
    match bind_listener(new_addr) {
        Ok(new_listener) => {
            *listener = Some(new_listener);
            *addr = new_addr;
            if let Ok(mut active) = current.lock() {
                *active = new_addr;
            }
            let _ = reply.send(Ok(()));
        }
        Err(error) => {
            if let Ok(old_listener) = bind_listener(*addr) {
                *listener = Some(old_listener);
            }
            let _ = reply.send(Err(error.to_string()));
        }
    }
}

fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr)
}

fn handle(mut stream: TcpStream, config: PathBuf, lan: Arc<Mutex<Lan>>) {
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
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let mut first = true;
    loop {
        let request = match read_request(&mut stream) {
            Ok(request) => request,
            Err(response) => {
                // An idle keep-alive connection times out or closes; do not
                // manufacture a 400 response for a request that never arrived.
                if first {
                    let _ = stream.write_all(&response);
                }
                return;
            }
        };
        first = false;
        if handle_stream(&mut stream, &config, &request, &lan) {
            return;
        }
        let keep_alive = wants_keep_alive(&request);
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

/// HTTP/1.1 defaults to keep-alive unless the client opts out; HTTP/1.0 is the
/// opposite unless the client opts in. Pipelining is not supported: the next
/// request is read after the previous response is written.
fn wants_keep_alive(request: &HttpRequest) -> bool {
    let connection = request_header(request, "connection")
        .unwrap_or("")
        .to_ascii_lowercase();
    let close = connection.split(',').any(|token| token.trim() == "close");
    if close {
        return false;
    }
    if request.version.eq_ignore_ascii_case("HTTP/1.0") {
        return connection
            .split(',')
            .any(|token| token.trim() == "keep-alive");
    }
    true
}

fn keep_alive_response(mut response: Vec<u8>) -> Vec<u8> {
    const CLOSE: &[u8] = b"Connection: close\r\n";
    const KEEP: &[u8] = b"Connection: keep-alive\r\n";
    if let Some(position) = response
        .windows(CLOSE.len())
        .position(|window| window == CLOSE)
    {
        response.splice(position..position + CLOSE.len(), KEEP.iter().copied());
    }
    response
}

/// Serve the two streaming UI endpoints before the buffered router. Returns
/// false when the path is not a UI stream so `respond` keeps its existing
/// routes and error text.
fn handle_stream(
    stream: &mut TcpStream,
    config: &Path,
    request: &HttpRequest,
    lan: &Arc<Mutex<Lan>>,
) -> bool {
    let path = request
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(request.path.as_str());
    let Some(ui) = ui_path(path) else {
        return false;
    };
    if ui.sub != "events" && ui.sub != "ws" {
        return false;
    }
    if request.method != "GET" {
        let _ = stream.write_all(&method_not_allowed("GET"));
        return true;
    }
    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&request.headers) {
            Ok(auth) => auth,
            Err(()) => {
                let _ = stream.write_all(&error(401, "unauthorized", "authentication required"));
                return true;
            }
        },
        Err(_) => {
            let _ = stream.write_all(&error(500, "internal", "LAN state unavailable"));
            return true;
        }
    };
    if !package::valid_name(ui.id) || plugin::web_assets(ui.id).is_none() {
        let _ = stream.write_all(&error(404, "not_found", "unknown plugin UI"));
        return true;
    }

    let origin = request_header(request, "origin");
    if origin.is_some() && !http::same_origin(&request.headers) {
        let _ = stream.write_all(&error(
            403,
            "forbidden",
            "cross-origin calls are not allowed",
        ));
        return true;
    }

    if ui.sub == "ws" {
        if auth == AuthKind::Cookie && origin.is_none() {
            let _ = stream.write_all(&error(
                403,
                "forbidden",
                "Origin is required for cookie-authenticated WebSocket calls",
            ));
            return true;
        }
        let accept = match uichannel::ws_accept(&request.headers) {
            Ok(accept) => accept,
            Err(message) => {
                let response = http::response_with_headers(
                    426,
                    "text/plain; charset=utf-8",
                    &[
                        ("Upgrade".to_string(), "websocket".to_string()),
                        ("Sec-WebSocket-Version".to_string(), "13".to_string()),
                    ],
                    message.as_bytes(),
                );
                let _ = stream.write_all(&response);
                return true;
            }
        };
        let (receiver, subscription) = match uichannel::bus().subscribe(ui.id) {
            Ok(subscription) => subscription,
            Err(_) => {
                let _ = stream.write_all(&error(
                    429,
                    "too_many_subscribers",
                    "too many UI subscribers for this plugin",
                ));
                return true;
            }
        };
        let handshake = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept
        );
        if stream.write_all(handshake.as_bytes()).is_err() {
            return true;
        }
        let _ = uichannel::serve_ws(stream, config, ui.id, receiver, subscription);
        return true;
    }

    let last_event_id =
        request_header(request, "last-event-id").and_then(|value| value.trim().parse::<u64>().ok());
    let (receiver, subscription, replay) =
        match uichannel::bus().subscribe_from(ui.id, last_event_id) {
            Ok(subscription) => subscription,
            Err(_) => {
                let _ = stream.write_all(&error(
                    429,
                    "too_many_subscribers",
                    "too many UI subscribers for this plugin",
                ));
                return true;
            }
        };
    let _ = uichannel::serve_sse(stream, receiver, replay);
    drop(subscription);
    true
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
        let token = match lan.lock() {
            Ok(state) => state.token().map(str::to_string),
            Err(_) => return error(500, "internal", "LAN state unavailable"),
        };
        return mcp_http::handle(
            config,
            &mcp_http::Request {
                method: &request.method,
                headers: &request.headers,
                body: &request.body,
            },
            token.as_deref(),
        );
    }

    let ui = ui_path(path);
    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&request.headers) {
            Ok(auth) => auth,
            Err(()) => return error(401, "unauthorized", "authentication required"),
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
        // A slashless URL never carries the `/plugin-ui/` session cookie, so
        // browsers would stall at 401. Redirect after auth: the session check
        // stays authoritative and only reachable pages redirect.
        if request.method == "GET"
            && ui.slashless
            && package::valid_name(ui.id)
            && plugin::web_assets(ui.id).is_some()
        {
            return http::response_with_headers(
                308,
                "text/plain; charset=utf-8",
                &[("Location".to_string(), format!("/plugin-ui/{}/", ui.id))],
                b"",
            );
        }
        return plugin_ui(config, request, &ui, auth);
    }
    route(config, request, lan)
}

/// One parsed `/plugin-ui/...` path.
struct UiPath<'a> {
    /// The plugin id segment (still unvalidated).
    id: &'a str,
    /// Everything after `/plugin-ui/{id}/`; empty for the plugin root.
    sub: &'a str,
    /// True for `/plugin-ui/{id}` without the trailing slash.
    slashless: bool,
}

fn ui_path(path: &str) -> Option<UiPath<'_>> {
    let rest = path.strip_prefix("/plugin-ui/")?;
    Some(match rest.split_once('/') {
        Some((id, sub)) => UiPath {
            id,
            sub,
            slashless: false,
        },
        None => UiPath {
            id: rest,
            sub: "",
            slashless: true,
        },
    })
}

/// Serve `/plugin-ui/{id}/...`: the plugin's package-local web assets, the
/// `/rpc` JSON-RPC bridge, and the SSE/WebSocket realtime streams. A plugin
/// never runs an HTTP listener of its own.
fn plugin_ui(config: &Path, request: &HttpRequest, ui: &UiPath<'_>, auth: AuthKind) -> Vec<u8> {
    if !package::valid_name(ui.id) {
        return error(404, "not_found", "unknown plugin UI");
    }
    let Some(assets) = plugin::web_assets(ui.id) else {
        return error(404, "not_found", &format!("plugin {} has no web UI", ui.id));
    };
    if ui.sub == "rpc" {
        return ui_rpc(config, request, ui.id, auth);
    }
    serve_ui(request, &assets, ui.id, ui.sub)
}

fn serve_ui(request: &HttpRequest, assets: &plugin::WebAssets, id: &str, sub: &str) -> Vec<u8> {
    if request.method != "GET" {
        return method_not_allowed("GET");
    }
    let Some(relative) = webui::relative_path(sub, &assets.entry) else {
        return error(404, "not_found", "not found");
    };
    if let Some((path, metadata)) = web_file(&assets.root, &relative) {
        return serve_web_file(request, &path, &metadata, id);
    }
    // SPA fallback: only an HTML navigation falls back to the entry. A missing
    // script must stay a 404 so the browser reports it instead of running HTML.
    if wants_html(request) {
        if let Some((path, metadata)) = web_file(&assets.root, &assets.entry) {
            return serve_web_file(request, &path, &metadata, id);
        }
    }
    error(404, "not_found", "not found")
}

/// Resolve a relative path under the web root and make sure it stays inside it
/// (the installer rejects symlinks, but this is cheap defence in depth).
fn web_file(root: &Path, relative: &str) -> Option<(PathBuf, fs::Metadata)> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        path.push(segment);
    }
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    if !path.starts_with(&root) {
        return None;
    }
    let metadata = fs::metadata(&path).ok()?;
    metadata.is_file().then_some((path, metadata))
}

fn serve_web_file(
    request: &HttpRequest,
    path: &Path,
    metadata: &fs::Metadata,
    id: &str,
) -> Vec<u8> {
    let mime = http::content_type(&path.to_string_lossy());
    let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
    let etag = webui::etag(modified, metadata.len());
    let headers = web_headers(&etag);
    if webui::not_modified(request_header(request, "if-none-match"), &etag) {
        return http::response_with_headers(304, mime, &headers, b"");
    }
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(cause) => return error(500, "internal", &format!("read UI asset: {}", cause)),
    };
    let body = if mime.starts_with("text/html") {
        webui::inject_base(&data, id)
    } else {
        data
    };
    http::response_with_headers(200, mime, &headers, &body)
}

fn web_headers(etag: &str) -> Vec<(String, String)> {
    vec![
        ("ETag".to_string(), etag.to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("Vary".to_string(), "Accept".to_string()),
        ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
    ]
}

fn wants_html(request: &HttpRequest) -> bool {
    request_header(request, "accept")
        .map(|value| value.to_ascii_lowercase().contains("text/html"))
        .unwrap_or(false)
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
    let token = request_header(request, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| {
            serde_json::from_slice::<Value>(&request.body)
                .ok()
                .and_then(|value| {
                    value
                        .get("token")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        });
    let Some(token) = token else {
        return error(401, "unauthorized", "invalid token");
    };
    let session = match lan.lock() {
        Ok(mut state) => match state.login(&token) {
            Ok(session) => session,
            Err(message) => return error(401, "unauthorized", &message),
        },
        Err(_) => return error(500, "internal", "LAN state unavailable"),
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

struct HttpRequest {
    method: String,
    path: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if buffer.len() > MAX_HEADER {
            return Err(error(400, "bad_request", "header is too large"));
        }
        let read = stream
            .read(&mut chunk)
            .map_err(|_| error(400, "bad_request", "cannot read request"))?;
        if read == 0 {
            return Err(error(400, "bad_request", "empty request"));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
    };

    let request = http::parse(&buffer[..header_end + 4])
        .ok_or_else(|| error(400, "bad_request", "malformed request line"))?;
    let version = std::str::from_utf8(&buffer[..header_end])
        .ok()
        .and_then(|head| head.lines().next())
        .and_then(|line| line.split_whitespace().nth(2))
        .unwrap_or("HTTP/1.1")
        .to_string();
    let content_length = header(&request, "content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if header(&request, "transfer-encoding").is_some() {
        return Err(error(
            400,
            "bad_request",
            "chunked bodies are not supported",
        ));
    }
    let limit = if request.path.starts_with("/api/v1/plugins")
        || request.path.starts_with("/api/v1/import")
    {
        MAX_BODY
    } else {
        MAX_JSON_BODY
    };
    if content_length > limit {
        return Err(error(413, "payload_too_large", "request body is too large"));
    }

    let mut body = buffer[header_end + 4..].to_vec();
    while (body.len() as u64) < content_length {
        let read = stream
            .read(&mut chunk)
            .map_err(|_| error(400, "bad_request", "cannot read request body"))?;
        if read == 0 {
            return Err(error(400, "bad_request", "truncated request body"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length as usize);

    Ok(HttpRequest {
        method: request.method.to_string(),
        path: request.path.to_string(),
        version,
        headers: request
            .headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body,
    })
}

fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    http::header(&request.headers, name)
}

fn header<'a>(request: &'a http::Request<'_>, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| *value)
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn route(config: &std::path::Path, request: &HttpRequest, lan_state: &Arc<Mutex<Lan>>) -> Vec<u8> {
    let (path, query) = request
        .path
        .split_once('?')
        .unwrap_or((request.path.as_str(), ""));
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() < 3 || parts[0] != "api" || parts[1] != "v1" {
        return error(404, "not_found", "unknown endpoint");
    }

    match (request.method.as_str(), parts.as_slice()) {
        ("GET", ["api", "v1", "version"]) => ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "protocol": 1,
            "platform": package::target(),
        })),
        ("GET", ["api", "v1", "lan"]) => match lan_state.lock() {
            Ok(state) => ok(json!({
                "enabled": state.enabled,
                "bind": state.bind.to_string(),
                "tokenSet": state.token().is_some(),
            })),
            Err(_) => error(500, "internal", "LAN state unavailable"),
        },
        ("GET", ["api", "v1", "services"]) => match rpc(config, "svc.list", json!({})) {
            Ok(value) => {
                let services = value
                    .get("services")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let summaries: Vec<Value> = services
                    .into_iter()
                    .filter_map(|status| status.get("summary").cloned())
                    .collect();
                ok(json!({ "services": summaries }))
            }
            Err(response) => response,
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
        ("POST", ["api", "v1", "plugins"]) => install_plugin(config, request, query),
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
        ("GET", ["api", "v1", "export", id]) => export_spkb(config, &decode(id)),
        (_, ["api", "v1", "export", _]) => method_not_allowed("GET"),
        ("POST", ["api", "v1", "import"]) => import_plugin(request, query),
        (_, ["api", "v1", "import"]) => method_not_allowed("POST"),
        ("GET", ["api", "v1", "logs", id]) => logs(config, &decode(id), query),
        (_, ["api", "v1", "logs", _]) => method_not_allowed("GET"),
        _ => error(404, "not_found", "unknown endpoint"),
    }
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
    let strategy = query_value(query, "strategy").unwrap_or_else(|| "keep-old".into());
    if strategy != "keep-old" && strategy != "replace" {
        return error(400, "bad_request", "strategy must be keep-old or replace");
    }
    let enable = query_bool(query, "enable").unwrap_or(true);

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
        json!({ "path": path.to_string_lossy(), "strategy": strategy }),
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

fn require_plugin(config: &Path, id: &str) -> Result<(), Vec<u8>> {
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
    // The HTTP surface is already authorised by the LAN token / loopback
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
    if text.len() <= MAX_LOG_BODY {
        return text;
    }
    let mut start = text.len() - MAX_LOG_BODY;
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

fn rpc(config: &std::path::Path, method: &str, params: Value) -> Result<Value, Vec<u8>> {
    let response = channel::request(config, method, params)
        .map_err(|cause| error(500, "internal", &format!("svcd unreachable: {}", cause)))?;
    if let Some(error) = response.error {
        return Err(map_error(&error.message));
    }
    response
        .result
        .ok_or_else(|| error(500, "internal", "empty daemon response"))
}

fn map_error(message: &str) -> Vec<u8> {
    let lower = message.to_ascii_lowercase();
    let status = if lower.contains("timed out") || lower.contains("timeout") {
        504
    } else if lower.contains("unknown") || lower.contains("not found") {
        404
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
    {
        400
    } else {
        500
    };
    error(status, code_for(status), message)
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
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}
