//! Minimal `/api/v1` HTTP management API and LAN authentication.
//!
//! The listener starts on loopback; `svc lan enable` can rebind it to a LAN
//! address and turns on bearer/cookie auth. Every request is translated into the
//! same daemon RPC the CLI uses, so the HTTP surface never owns a second copy of
//! service state.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
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
use crate::package;
use crate::paths;

const DEFAULT_PORT: u16 = 8787;
const MAX_BODY: u64 = 512 * 1024 * 1024;
const MAX_JSON_BODY: u64 = 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub struct Api {
    config: PathBuf,
    port: u16,
    lan: Arc<Mutex<Lan>>,
    commands: Sender<Command>,
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
    listener.set_nonblocking(true)?;
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
    let api = Arc::new(Api {
        config: config.clone(),
        port: local.port(),
        lan: lan.clone(),
        commands,
    });
    thread::Builder::new()
        .name("svcd-http".into())
        .spawn(move || listener_loop(listener, local, receiver, lan, config))?;
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
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(Command::Rebind { addr, reply })
            .map_err(|_| "HTTP listener is not running".to_string())?;
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "HTTP rebind timed out".to_string())?
    }
}

fn listener_loop(
    listener: TcpListener,
    addr: SocketAddr,
    receiver: Receiver<Command>,
    lan: Arc<Mutex<Lan>>,
    config: PathBuf,
) {
    let mut listener = Some(listener);
    let mut addr = addr;
    loop {
        while let Ok(Command::Rebind {
            addr: new_addr,
            reply,
        }) = receiver.try_recv()
        {
            drop(listener.take());
            match bind_listener(new_addr) {
                Ok(new_listener) => {
                    listener = Some(new_listener);
                    addr = new_addr;
                    let _ = reply.send(Ok(()));
                }
                Err(error) => {
                    if let Ok(old_listener) = bind_listener(addr) {
                        listener = Some(old_listener);
                    }
                    let _ = reply.send(Err(error.to_string()));
                }
            }
        }
        if let Some(current) = listener.as_ref() {
            loop {
                match current.accept() {
                    Ok((stream, _)) => {
                        let lan = lan.clone();
                        let config = config.clone();
                        let _ = thread::Builder::new()
                            .name("svcd-http-conn".into())
                            .spawn(move || handle(stream, config, lan));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn handle(mut stream: TcpStream, config: PathBuf, lan: Arc<Mutex<Lan>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(response) => {
            let _ = stream.write_all(&response);
            return;
        }
    };
    let response = respond(&config, &request, &lan);
    let _ = stream.write_all(&response);
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

    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&request.headers) {
            Ok(auth) => auth,
            Err(()) => return error(401, "unauthorized", "authentication required"),
        },
        Err(_) => return error(500, "internal", "LAN state unavailable"),
    };
    if auth == AuthKind::Cookie && is_write(&request.method) && !csrf_ok(request) {
        return error(
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        );
    }
    route(config, request, lan)
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
    let limit = if request.path.starts_with("/api/v1/plugins") {
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
        headers: request
            .headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        body,
    })
}

fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
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
    let body = json!({ "error": { "code": code, "message": message } });
    http::response(
        status,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
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
