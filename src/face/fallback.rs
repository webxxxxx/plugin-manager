//! S4 回退路由：线程驱动把解析后的 request 交给这里。
//!
//! 这些是 face 的端点投影与请求谓词（MCP 之外的路由、插件 UI 的静态资源与
//! rpc、blob/file 会话路由、CSRF/Origin 判断）。连接与 socket 归 `api.rs` 的
//! 回退 loop；这里只拿到 request 值，不拥有连接。

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::face::blob_routes;
use crate::face::plugin_ui::{ui_path, UiPath};
use crate::face::support::{decode, error};
use crate::face::webui;
use crate::manage::package;
use crate::manage::plugin;
use crate::platform::config::Config;
use crate::platform::storage::blob;
use crate::server::auth::admission::{Admission, Rejected};
use crate::server::auth::lan::{AuthKind, Lan};
use crate::server::http::engine::HttpRequest;
use crate::server::http::model as http;
use crate::wire::control::channel;
use crate::wire::message::rpc;

/// Serve the paths that still own their protocol: MCP, plugin UI rpc/static
/// assets, and the transfer fallbacks the face deliberately leaves alone.
pub(crate) fn respond(config: &Config, request: &HttpRequest, lan: &Arc<Mutex<Lan>>) -> Vec<u8> {
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
        return crate::server::mcp::transport::handle(
            &|request| crate::face::mcp::serve_modern(&config.paths.config_dir, request),
            &crate::server::mcp::transport::Request {
                method: &request.method,
                headers: &request.headers,
                body: &request.body,
            },
            password_hash.as_deref(),
        );
    }

    let ui = ui_path(path);
    let auth = match Admission::new(lan).admit(&request.headers) {
        Ok(auth) => auth,
        Err(Rejected::Unavailable) => return error(500, "internal", "LAN state unavailable"),
        Err(Rejected::Unauthorized) => {
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
    };
    // `/plugin-ui/{id}/rpc` keeps the same-origin Origin check instead of the
    // management API's custom CSRF header, so a plugin's own page can call it
    // with a plain `fetch` (see `ui_rpc`).
    if auth == AuthKind::Cookie
        && http::is_write_method(&request.method)
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
            && plugin::web_assets(&config.paths, ui.id).is_some()
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
    config: &Config,
    request: &HttpRequest,
    ui: &UiPath<'_>,
    auth: AuthKind,
    lan: &Arc<Mutex<Lan>>,
) -> Vec<u8> {
    if !package::valid_name(ui.id) {
        return error(404, "not_found", "unknown plugin UI");
    }
    let Some(assets) = plugin::web_assets(&config.paths, ui.id) else {
        return error(404, "not_found", &format!("plugin {} has no web UI", ui.id));
    };
    if !token_valid(lan, ui.token, ui.id) {
        return error(401, "unauthorized", "invalid or expired plugin token");
    }
    if ui.sub == "rpc" {
        return ui_rpc(&config.paths.config_dir, request, ui.id, auth);
    }
    let base = format!("/plugin-ui/{}/{}/", ui.id, ui.token);
    serve_ui(request, &assets, ui.sub, &base)
}

fn token_valid(lan: &Arc<Mutex<Lan>>, token: &str, plugin: &str) -> bool {
    crate::server::auth::admission::plugin_token_valid(lan, token, plugin)
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

fn csrf_ok(request: &HttpRequest) -> bool {
    http::csrf_ok(&request.headers)
}

fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    http::header(&request.headers, name)
}

/// The buffered routes served by the fallback handler are the blob/file
/// session routes (#118/#119); every management route and every streamed
/// transfer is claimed before this function runs.
fn route(config: &Config, request: &HttpRequest) -> Vec<u8> {
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
            blob_routes::create_upload(&config.paths, blob::UploadKind::Blob)
        }
        ("GET", ["api", "v1", "blobs", "uploads", id]) => {
            blob_routes::upload_status(&config.paths, blob::UploadKind::Blob, &decode(id))
        }
        ("POST", ["api", "v1", "blobs", "uploads", id, "complete"]) => {
            blob_routes::complete_upload(&config.paths, &decode(id), &request.body)
        }
        ("DELETE", ["api", "v1", "blobs", "uploads", id]) => {
            blob_routes::abort_upload(&config.paths, blob::UploadKind::Blob, &decode(id))
        }
        (_, ["api", "v1", "blobs"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads", _]) => method_not_allowed("GET, DELETE"),
        (_, ["api", "v1", "blobs", "uploads", _, "complete"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads", _, "parts", _]) => method_not_allowed("PUT"),
        (_, ["api", "v1", "blobs", _]) => method_not_allowed("GET"),
        ("POST", ["api", "v1", "file", "uploads"]) => {
            blob_routes::create_upload(&config.paths, blob::UploadKind::File)
        }
        ("GET", ["api", "v1", "file", "uploads", id]) => {
            blob_routes::upload_status(&config.paths, blob::UploadKind::File, &decode(id))
        }
        ("POST", ["api", "v1", "file", "uploads", id, "complete"]) => {
            blob_routes::file_upload_complete(&config.paths, &decode(id), &request.body)
        }
        ("DELETE", ["api", "v1", "file", "uploads", id]) => {
            blob_routes::abort_upload(&config.paths, blob::UploadKind::File, &decode(id))
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
