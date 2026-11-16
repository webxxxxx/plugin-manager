//! `/api/v1` face: URL → capability projection.
//!
//! #113 moved `GET /api/v1/version` through the S4 seam, #116 added the first
//! `Blocking` route, and #117 moves the remaining management JSON routes here.
//! Blob/file transfer, export spk/spkb, import/install and the plugin UI
//! streams keep their own streaming paths until #118–#122; this face owns the
//! ordinary management surface, the console page and login/logout.

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::http;
use crate::lan::{self, AuthKind, Lan};
use crate::log;
use crate::package;
use crate::paths;
use crate::plugin;
use crate::server::http::{
    BodyPolicy, Completion, Dispatch, Exchange, Face, Request, Response, Route,
};

const UI: &str = include_str!("../web/index.html");

/// The management API face.
pub struct ApiFace {
    config: PathBuf,
    lan: Arc<Mutex<Lan>>,
}

impl ApiFace {
    pub fn new(config: impl Into<PathBuf>, lan: Arc<Mutex<Lan>>) -> ApiFace {
        ApiFace {
            config: config.into(),
            lan,
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value, Response> {
        crate::api::rpc_result(&self.config, method, params).map_err(rpc_error)
    }

    fn require_plugin(&self, id: &str) -> Result<(), Response> {
        crate::api::require_plugin_result(&self.config, id)
            .map_err(|(status, code, message)| error_response(status, code, &message))
    }

    /// Project `svc.list` into the summaries the console consumes.
    fn list_services(&self) -> Response {
        match self.rpc("svc.list", json!({})) {
            Ok(value) => {
                let services: Vec<Value> = value
                    .get("services")
                    .and_then(Value::as_array)
                    .map(|services| services.iter().filter_map(project_service).collect())
                    .unwrap_or_default();
                Response::json(200, &json!({ "services": services }))
            }
            Err(response) => response,
        }
    }

    fn service_status(&self, id: &str) -> Response {
        match self.rpc("svc.status", json!({ "id": id })) {
            Ok(status) => Response::json(200, &status),
            Err(response) => response,
        }
    }

    fn service_action(&self, method: &str, id: &str) -> Response {
        match self.rpc(method, json!({ "id": id })) {
            Ok(status) => summary_response(status),
            Err(response) => response,
        }
    }

    fn plugin_ui_token(&self, id: &str) -> Response {
        if !package::valid_name(id) {
            return error_response(404, "not_found", &format!("unknown plugin UI: {}", id));
        }
        if plugin::web_assets(id).is_none() {
            return error_response(404, "not_found", &format!("plugin {} has no web UI", id));
        }
        let token = match self.lan.lock() {
            Ok(mut state) => state.issue_plugin_token(id),
            Err(_) => return error_response(500, "internal", "auth state unavailable"),
        };
        Response::json(
            200,
            &json!({
                "token": token,
                "url": format!("/plugin-ui/{}/{}/", id, token),
                "expiresIn": lan::plugin_token_ttl_secs(),
            }),
        )
    }

    fn uninstall_plugin(&self, id: &str, purge: bool) -> Response {
        match self.rpc("plugin.uninstall", json!({ "id": id, "purge": purge })) {
            Ok(_) => Response::json(200, &json!({ "ok": true })),
            Err(response) => response,
        }
    }

    fn get_config(&self, id: &str) -> Response {
        if let Err(response) = self.require_plugin(id) {
            return response;
        }
        let Some(path) = plugin::active_config(id) else {
            return error_response(404, "not_found", &format!("plugin {} has no config", id));
        };
        match std::fs::read(&path) {
            Ok(data) => Response::new(200, "text/plain", data),
            Err(cause) => error_response(500, "internal", &format!("read config: {}", cause)),
        }
    }

    fn put_config(&self, id: &str, body: &[u8]) -> Response {
        if let Err(response) = self.require_plugin(id) {
            return response;
        }
        let _guard = plugin::config_lock();
        let _lock = match plugin::lock_config(id) {
            Ok(lock) => lock,
            Err(message) => return error_response(500, "internal", &message),
        };
        match plugin::replace_config(id, body, true) {
            Ok(()) => Response::json(200, &json!({ "ok": true })),
            Err(message) => config_error_response(&message),
        }
    }

    fn reset_config(&self, id: &str) -> Response {
        if let Err(response) = self.require_plugin(id) {
            return response;
        }
        let Some((template, _)) = plugin::config_template(id) else {
            return error_response(
                404,
                "not_found",
                &format!("plugin {} has no config template", id),
            );
        };
        let _guard = plugin::config_lock();
        let _lock = match plugin::lock_config(id) {
            Ok(lock) => lock,
            Err(message) => return error_response(500, "internal", &message),
        };
        match plugin::replace_config(id, &template, true) {
            Ok(()) => Response::json(200, &json!({ "ok": true })),
            Err(message) => config_error_response(&message),
        }
    }

    fn export_config(&self, id: &str) -> Response {
        if let Err(response) = self.require_plugin(id) {
            return response;
        }
        match plugin::export_config(id) {
            Ok((version, data)) => download_response(
                &format!("{}-{}.config", safe_filename(id), safe_filename(&version)),
                &data,
            ),
            Err(message) if message.contains("has no config") => {
                error_response(404, "not_found", &message)
            }
            Err(message) => error_response(500, "internal", &message),
        }
    }

    fn trust_list(&self) -> Response {
        match crate::trust::keys() {
            Ok(keys) => Response::json(
                200,
                &json!({
                    "keys": keys,
                    "directory": paths::trust_dir().to_string_lossy(),
                }),
            ),
            Err(cause) => error_response(500, "internal", &cause),
        }
    }

    fn trust_add(&self, body: &[u8], query: &str) -> Response {
        let Some(key_id) = crate::api::query_value(query, "key_id") else {
            return error_response(400, "bad_request", "key_id is required");
        };
        if body.is_empty() {
            return error_response(400, "bad_request", "empty public key");
        }
        match crate::trust::add(&key_id, body) {
            Ok(path) => Response::json(
                200,
                &json!({
                    "keyId": key_id,
                    "path": path.to_string_lossy(),
                }),
            ),
            Err(error) => map_error_response(&error),
        }
    }

    fn trust_remove(&self, key_id: &str) -> Response {
        match crate::trust::remove(key_id) {
            Ok(true) => Response::json(200, &json!({ "keyId": key_id, "removed": true })),
            Ok(false) => error_response(
                404,
                "not_found",
                &format!("unknown trusted key: {}", key_id),
            ),
            Err(error) => map_error_response(&error),
        }
    }

    fn logs(&self, id: &str, query: &str) -> Response {
        if let Err(response) = self.require_plugin(id) {
            return response;
        }
        let lines = match crate::api::query_value(query, "lines") {
            None => 200,
            Some(value) => match value.parse::<usize>() {
                Ok(lines) if lines <= 2000 => lines,
                _ => {
                    return error_response(
                        400,
                        "bad_request",
                        "lines must be an integer between 0 and 2000",
                    )
                }
            },
        };
        let path = paths::logs_dir().join(format!("{}.log", id));
        match log::tail(&path, lines) {
            Ok(text) => Response::new(
                200,
                "text/plain; charset=utf-8",
                cap_log(text).into_bytes(),
            ),
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
                error_response(404, "not_found", &format!("no log for {}", id))
            }
            Err(cause) => error_response(500, "internal", &format!("cannot read log: {}", cause)),
        }
    }

    fn login(&self, request: &Request) -> Response {
        let password = http::header(&request.headers, "authorization")
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
            return error_response(401, "unauthorized", "password required");
        };
        let session = match self.lan.lock() {
            Ok(mut state) => match state.login(&password) {
                Ok(session) => session,
                Err(message) => return error_response(401, "unauthorized", &message),
            },
            Err(_) => return error_response(500, "internal", "auth state unavailable"),
        };
        let mut response = Response::json(200, &json!({ "ok": true }));
        for (name, value) in lan::cookie_headers(&session) {
            response = response.with_header(name, value);
        }
        response
    }

    fn logout(&self, request: &Request) -> Response {
        if let Ok(mut state) = self.lan.lock() {
            state.logout(&request.headers);
        }
        let mut response = Response::json(200, &json!({ "ok": true }));
        for (name, value) in lan::clear_cookie_headers() {
            response = response.with_header(name, value);
        }
        response
    }

    fn handle_api(&self, request: &Request) -> Response {
        let (path, query) = request
            .path
            .split_once('?')
            .unwrap_or((request.path.as_str(), ""));
        let mut parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.get(2) == Some(&"files") {
            parts[2] = "file";
        }
        match (request.method.as_str(), parts.as_slice()) {
            ("GET", ["api", "v1", "compat"]) => Response::json(
                200,
                &serde_json::to_value(crate::compat::detect()).unwrap_or(Value::Null),
            ),
            ("GET", ["api", "v1", "auth"]) => match self.lan.lock() {
                Ok(state) => Response::json(
                    200,
                    &json!({
                        "bind": state.bind.to_string(),
                        "requestedBind": state.requested_bind.to_string(),
                        "authRequired": state.require_auth,
                        "passwordSet": state.password_hash().is_some(),
                    }),
                ),
                Err(_) => error_response(500, "internal", "auth state unavailable"),
            },
            ("GET", ["api", "v1", "version"]) => Response::json(
                200,
                &json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": 1,
                    "platform": crate::package::target(),
                }),
            ),
            ("GET", ["api", "v1", "services"]) => self.list_services(),
            ("GET", ["api", "v1", "services", id]) => self.service_status(&decode(id)),
            ("POST", ["api", "v1", "services", id, "start"]) => {
                self.service_action("svc.start", &decode(id))
            }
            ("POST", ["api", "v1", "services", id, "stop"]) => {
                self.service_action("svc.stop", &decode(id))
            }
            ("POST", ["api", "v1", "services", id, "enable"]) => {
                self.service_action("svc.enable", &decode(id))
            }
            ("POST", ["api", "v1", "services", id, "disable"]) => {
                self.service_action("svc.disable", &decode(id))
            }
            ("POST", ["api", "v1", "plugins", id, "ui-token"]) => {
                self.plugin_ui_token(&decode(id))
            }
            (_, ["api", "v1", "plugins", _, "ui-token"]) => method_not_allowed("POST"),
            ("DELETE", ["api", "v1", "plugins", id]) => {
                let purge = crate::api::query_bool(query, "purge").unwrap_or(false);
                self.uninstall_plugin(&decode(id), purge)
            }
            ("POST", ["api", "v1", "config", id, "reset"]) => self.reset_config(&decode(id)),
            (_, ["api", "v1", "config", _, "reset"]) => method_not_allowed("POST"),
            ("GET", ["api", "v1", "config", id]) => self.get_config(&decode(id)),
            ("PUT", ["api", "v1", "config", id]) => self.put_config(&decode(id), &request.body),
            (_, ["api", "v1", "config", _]) => method_not_allowed("GET, PUT"),
            ("GET", ["api", "v1", "export", id, "config"]) => self.export_config(&decode(id)),
            (_, ["api", "v1", "export", _, "config"]) => method_not_allowed("GET"),
            (_, ["api", "v1", "export", _, "spk"]) => method_not_allowed("GET"),
            (_, ["api", "v1", "export", _]) => method_not_allowed("GET"),
            (_, ["api", "v1", "import"]) => method_not_allowed("POST"),
            ("GET", ["api", "v1", "trust"]) => self.trust_list(),
            ("POST", ["api", "v1", "trust"]) => self.trust_add(&request.body, query),
            ("DELETE", ["api", "v1", "trust", key]) => self.trust_remove(&decode(key)),
            (_, ["api", "v1", "trust"]) => method_not_allowed("GET, POST"),
            (_, ["api", "v1", "trust", _]) => method_not_allowed("DELETE"),
            ("GET", ["api", "v1", "logs", id]) => self.logs(&decode(id), query),
            (_, ["api", "v1", "logs", _]) => method_not_allowed("GET"),
            _ => error_response(404, "not_found", "unknown endpoint"),
        }
    }
}

fn path_only(path: &str) -> &str {
    path.split_once('?').map(|(path, _)| path).unwrap_or(path)
}

fn parts(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}

fn is_version(method: &str, path: &str) -> bool {
    method == "GET" && path_only(path).trim_matches('/') == "api/v1/version"
}

fn is_login(method: &str, path: &str) -> bool {
    method == "POST" && path_only(path) == "/login"
}

fn is_logout(method: &str, path: &str) -> bool {
    method == "POST" && path_only(path) == "/logout"
}

fn is_console(path: &str) -> bool {
    matches!(path_only(path), "/" | "/index.html")
}

fn is_api_path(path: &str) -> bool {
    let parts = parts(path_only(path));
    parts.first() == Some(&"api") && parts.get(1) == Some(&"v1")
}

/// Paths owned by the transfer slices (#118–#121) stay on the legacy bridge
/// until their own StreamHandler lands.
fn is_transfer_path(method: &str, path: &str) -> bool {
    let path = path_only(path);
    let mut parts = parts(path);
    if parts.get(2) == Some(&"files") {
        parts[2] = "file";
    }
    match parts.as_slice() {
        ["api", "v1", "plugins"] if method == "POST" => true,
        ["api", "v1", "import"] if method == "POST" => true,
        ["api", "v1", "blobs", ..] => true,
        ["api", "v1", "file", ..] => true,
        ["api", "v1", "export", _] if method == "GET" => true,
        ["api", "v1", "export", _, "spk"] if method == "GET" => true,
        _ => false,
    }
}

fn body_policy(method: &str, path: &str) -> BodyPolicy {
    let limits = crate::limits::current();
    if is_login(method, path) {
        return BodyPolicy::Buffered {
            limit: limits.max_json_body,
        };
    }
    let path = path_only(path);
    let mut parts = parts(path);
    if parts.get(2) == Some(&"files") {
        parts[2] = "file";
    }
    match (method, parts.as_slice()) {
        ("PUT", ["api", "v1", "config", _]) => BodyPolicy::Buffered {
            limit: limits.max_json_body,
        },
        ("POST", ["api", "v1", "trust"]) => BodyPolicy::Buffered {
            limit: limits.max_json_body,
        },
        _ => BodyPolicy::Empty,
    }
}

fn dispatch_for(method: &str, path: &str) -> Dispatch {
    if is_version(method, path) || is_console(path) {
        Dispatch::Inline
    } else {
        Dispatch::Blocking
    }
}

fn route_with(body: BodyPolicy, dispatch: Dispatch) -> Route {
    match dispatch {
        Dispatch::Inline => Route::inline(body),
        Dispatch::Blocking => Route::blocking(body),
    }
}

fn decode(value: &str) -> String {
    http::percent_decode(value, true)
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

fn error_response(status: u16, code: &str, message: &str) -> Response {
    Response::json(
        status,
        &json!({ "error": { "code": code, "message": message } }),
    )
}

fn rpc_error(failure: crate::api::RpcFailure) -> Response {
    let (status, code, message) = failure;
    error_response(status, code, &message)
}

fn map_error_response(message: &str) -> Response {
    let (status, code, message) = crate::api::map_error_status(message);
    error_response(status, code, &message)
}

fn config_error_response(message: &str) -> Response {
    let (status, code, message) = crate::api::config_error_status(message);
    error_response(status, code, &message)
}

fn method_not_allowed(allow: &str) -> Response {
    Response::json(
        405,
        &json!({
            "error": {
                "code": "method_not_allowed",
                "message": format!("method not allowed; use {}", allow),
            }
        }),
    )
    .with_header("Allow", allow)
}

fn summary_response(status: Value) -> Response {
    Response::json(
        200,
        &json!({
            "summary": status.get("summary").cloned().unwrap_or(Value::Null)
        }),
    )
}

fn download_response(filename: &str, data: &[u8]) -> Response {
    Response::new(200, "application/octet-stream", data)
        .with_header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .with_header("Cache-Control", "no-store")
}

fn safe_filename(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .collect()
}

fn project_service(status: &Value) -> Option<Value> {
    let has_config = status
        .pointer("/details/config")
        .and_then(Value::as_str)
        .is_some();
    let has_ui = status
        .get("summary")
        .and_then(|summary| summary.get("id"))
        .and_then(Value::as_str)
        .map(|id| crate::plugin::web_assets(id).is_some())
        .unwrap_or(false);
    let mut summary = status.get("summary").cloned()?;
    if let Some(object) = summary.as_object_mut() {
        object.insert("hasConfig".into(), Value::Bool(has_config));
        object.insert("hasUI".into(), Value::Bool(has_ui));
    }
    Some(summary)
}

impl Face for ApiFace {
    fn route(&self, method: &str, path: &str) -> Option<Route> {
        let body = body_policy(method, path);
        if is_login(method, path) || is_logout(method, path) || is_console(path) {
            return Some(route_with(body, dispatch_for(method, path)).public());
        }
        if !is_api_path(path) || is_transfer_path(method, path) {
            return None;
        }
        Some(route_with(body, dispatch_for(method, path)))
    }

    fn handle(&self, request: Request, done: Completion) {
        let response = if is_login(&request.method, &request.path) {
            self.login(&request)
        } else if is_logout(&request.method, &request.path) {
            self.logout(&request)
        } else if matches!(path_only(&request.path), "/" | "/index.html") {
            if request.method != "GET" {
                method_not_allowed("GET")
            } else {
                Response::new(200, "text/html; charset=utf-8", UI.as_bytes())
                    .with_header("Cache-Control", "no-store")
                    .with_header("X-Content-Type-Options", "nosniff")
            }
        } else if is_api_path(&request.path) {
            if request.principal == AuthKind::Cookie
                && crate::api::is_write(&request.method)
                && !crate::api::csrf_ok_headers(&request.headers)
            {
                error_response(
                    403,
                    "forbidden",
                    "cookie writes require Origin and X-Svcd-Csrf",
                )
            } else {
                self.handle_api(&request)
            }
        } else {
            error_response(500, "internal", "face handled a route it did not claim")
        };
        done.complete(Exchange::Response(response));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::http::dispatch_inline;
    use crate::server::http::{CompletionSlot, Exchange};

    fn lan() -> Arc<Mutex<Lan>> {
        let bind = "127.0.0.1:0".parse().expect("bind");
        Arc::new(Mutex::new(Lan::new(bind, bind, false, None)))
    }

    fn face() -> ApiFace {
        ApiFace::new(".", lan())
    }

    fn face_with_password() -> ApiFace {
        let bind = "127.0.0.1:0".parse().expect("bind");
        let hash = crate::auth::hash_password("secret").expect("hash");
        ApiFace::new(
            ".",
            Arc::new(Mutex::new(Lan::new(bind, bind, true, Some(hash)))),
        )
    }

    fn request(method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: Vec::new(),
            peer: "127.0.0.1".parse().expect("peer"),
            principal: AuthKind::None,
            body: Vec::new(),
        }
    }

    #[test]
    fn management_routes_have_the_expected_body_and_dispatch_policy() {
        let face = face();
        let login = face.route("POST", "/login").expect("login route");
        assert!(login.public);
        assert_eq!(login.dispatch, Dispatch::Blocking);
        assert!(matches!(login.body, BodyPolicy::Buffered { .. }));

        let logout = face.route("POST", "/logout").expect("logout route");
        assert!(logout.public);
        assert_eq!(logout.dispatch, Dispatch::Blocking);
        assert_eq!(logout.body, BodyPolicy::Empty);

        for path in ["/", "/index.html"] {
            let route = face.route("GET", path).expect("console route");
            assert!(route.public, "{path}");
            assert_eq!(route.dispatch, Dispatch::Inline);
        }

        let auth = face.route("GET", "/api/v1/auth").expect("auth route");
        assert!(!auth.public);
        assert_eq!(auth.dispatch, Dispatch::Blocking);

        for (method, path) in [
            ("GET", "/api/v1/compat"),
            ("GET", "/api/v1/services/demo"),
            ("POST", "/api/v1/services/demo/start"),
            ("POST", "/api/v1/plugins/demo/ui-token"),
            ("DELETE", "/api/v1/plugins/demo"),
            ("GET", "/api/v1/config/demo"),
            ("GET", "/api/v1/export/demo/config"),
            ("GET", "/api/v1/logs/demo"),
            ("GET", "/api/v1/nope"),
        ] {
            let route = face
                .route(method, path)
                .unwrap_or_else(|| panic!("{method} {path}"));
            assert!(!route.public, "{method} {path}");
            assert_eq!(route.dispatch, Dispatch::Blocking, "{method} {path}");
        }

        let put = face
            .route("PUT", "/api/v1/config/demo")
            .expect("config write");
        assert!(matches!(put.body, BodyPolicy::Buffered { .. }));
        let trust = face
            .route("POST", "/api/v1/trust")
            .expect("trust write");
        assert!(matches!(trust.body, BodyPolicy::Buffered { .. }));
    }

    #[test]
    fn transfer_and_stream_paths_stay_on_the_legacy_bridge() {
        let face = face();
        for (method, path) in [
            ("POST", "/api/v1/plugins"),
            ("POST", "/api/v1/import"),
            ("POST", "/api/v1/blobs/uploads"),
            ("GET", "/api/v1/blobs/abc"),
            ("PUT", "/api/v1/blobs/uploads/abc/parts/1"),
            ("GET", "/api/v1/file/abc"),
            ("POST", "/api/v1/file/uploads"),
            ("GET", "/api/v1/export/demo"),
            ("GET", "/api/v1/export/demo/spk"),
            ("GET", "/mcp"),
            ("GET", "/plugin-ui/demo/token/events"),
        ] {
            assert!(
                face.route(method, path).is_none(),
                "{method} {path} must stay legacy"
            );
        }
    }

    #[test]
    fn cookie_writes_without_csrf_are_rejected_before_the_projection() {
        let face = face();
        let mut req = request("POST", "/api/v1/trust");
        req.principal = AuthKind::Cookie;
        req.headers = vec![("Host".into(), "127.0.0.1:8787".into())];
        let (slot, done) = CompletionSlot::new("POST /api/v1/trust");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the face must answer");
        };
        assert_eq!(response.status, 403);
        let body: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(body["error"]["code"], "forbidden");

        // A bearer write is not the cookie CSRF surface.
        let mut req = request("POST", "/api/v1/trust");
        req.principal = AuthKind::Bearer;
        let (slot, done) = CompletionSlot::new("POST /api/v1/trust");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the face must answer");
        };
        assert_eq!(response.status, 400);
    }

    #[test]
    fn login_is_public_and_sets_a_session_cookie() {
        let face = face_with_password();
        let mut req = request("POST", "/login");
        req.body = br#"{"password":"secret"}"#.to_vec();
        let (slot, done) = CompletionSlot::new("POST /login");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("login must answer");
        };
        assert_eq!(response.status, 200);
        assert!(
            response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("set-cookie")
                    && value.contains(lan::COOKIE_SESSION)
                    && value.contains("Max-Age=86400")
            }),
            "login must set the session cookie: {:?}",
            response.headers
        );
        assert!(slot.take().is_none(), "the completion is exactly once");
    }

    #[test]
    fn logout_is_public_and_clears_the_cookie() {
        let face = face_with_password();
        let req = request("POST", "/logout");
        let (slot, done) = CompletionSlot::new("POST /logout");
        face.handle(req, done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("logout must answer");
        };
        assert_eq!(response.status, 200);
        assert!(
            response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("set-cookie")
                    && value.contains(lan::COOKIE_SESSION)
                    && value.contains("Max-Age=0")
            }),
            "logout must clear the session cookie"
        );
    }

    #[test]
    fn console_page_is_public_html() {
        let face = face();
        let (slot, done) = CompletionSlot::new("GET /");
        face.handle(request("GET", "/"), done);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the console must answer");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/html; charset=utf-8");
        assert!(
            String::from_utf8_lossy(&response.body).contains("id=\"rows\""),
            "the console table is missing"
        );
    }

    #[test]
    fn version_is_served_through_the_face_with_the_original_payload() {
        let face = face();
        let route = face.route("GET", "/api/v1/version").expect("route");
        let exchange = dispatch_inline(&face, &route, request("GET", "/api/v1/version"))
            .expect("version must complete");
        let Exchange::Response(response) = exchange else {
            panic!("version returns a memory response");
        };
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        let body: Value = serde_json::from_slice(&response.body).expect("json");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["protocol"], 1);
        assert_eq!(body["platform"], crate::package::target());
    }

    #[test]
    fn a_blocking_route_is_not_dispatched_inline() {
        let face = face();
        let route = face.route("GET", "/api/v1/services").expect("route");
        let error = match dispatch_inline(&face, &route, request("GET", "/api/v1/services")) {
            Ok(_) => panic!("a blocking route must not run on the server loop"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            crate::server::http::DispatchError::UnsupportedDispatch(Dispatch::Blocking)
        );
    }

    #[test]
    fn service_summaries_carry_has_config_and_has_ui() {
        let with_config = json!({
            "summary": { "id": "unit-test-missing", "kind": "plugin" },
            "details": { "config": "/tmp/config" }
        });
        let summary = project_service(&with_config).expect("summary");
        assert_eq!(summary["hasConfig"], true);
        assert_eq!(summary["hasUI"], false);

        let without_config = json!({
            "summary": { "id": "unit-test-missing", "kind": "core" }
        });
        let summary = project_service(&without_config).expect("summary");
        assert_eq!(summary["hasConfig"], false);
        assert_eq!(summary["hasUI"], false);
    }
}
