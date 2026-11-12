//! Minimal `/api/v1` HTTP management API.
//!
//! v1 binds loopback only and has no TLS; LAN bind and bearer/cookie auth land
//! in #38. Every request is translated into the same daemon RPC the CLI uses,
//! so the HTTP surface never owns a second copy of service state.

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::channel;
use crate::http;
use crate::log;
use crate::package;
use crate::paths;

const DEFAULT_PORT: u16 = 8787;
const MAX_BODY: u64 = 512 * 1024 * 1024;
const MAX_JSON_BODY: u64 = 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub fn spawn(config: PathBuf) -> std::io::Result<()> {
    let port = std::env::var("SVCD_HTTP_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let local = listener.local_addr()?;
    let port_file = config.join("http.port");
    fs::write(&port_file, format!("127.0.0.1:{}", local.port())).map_err(|error| {
        std::io::Error::other(format!("write {}: {}", port_file.display(), error))
    })?;
    log::svcd(&format!("svcd: HTTP API on 127.0.0.1:{}", local.port()));

    thread::Builder::new()
        .name("svcd-http".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let config = config.clone();
                let _ = thread::Builder::new()
                    .name("svcd-http-conn".into())
                    .spawn(move || handle(stream, config));
            }
        })?;
    Ok(())
}

fn handle(mut stream: TcpStream, config: PathBuf) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(response) => {
            let _ = stream.write_all(&response);
            return;
        }
    };
    let response = route(&config, &request);
    let _ = stream.write_all(&response);
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

fn route(config: &std::path::Path, request: &HttpRequest) -> Vec<u8> {
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
