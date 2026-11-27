//! S4 回退路由：回退处理器把解析后的 request 交给这里。
//!
//! 这些是 face 的端点投影与请求谓词（MCP 之外的路由、插件 UI 的静态资源与
//! rpc、blob/file 会话路由、CSRF/Origin 判断）。连接与 socket 归服务器循环；
//! 这里只拿到 request 值，不拥有连接。

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::face::blob_routes;
use crate::face::plugin_ui::{static_assets, ui_path, unauthenticated, web_assets_for, UiPath};
use crate::face::routes::{self, Endpoint, Owner};
use crate::face::support::{decode, error};
use crate::face::webui;
use crate::manage::WebAssets;
use crate::platform::config::Config;
use crate::platform::storage as blob;
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
            &|request| crate::face::mcp::serve_modern(config.paths.control_dir(), request),
            &crate::server::mcp::transport::Request {
                method: &request.method,
                headers: &request.headers,
                body: &request.body,
            },
            password_hash.as_deref(),
        );
    }

    let ui = ui_path(path);
    // The vendor-backend handoff is an authentication entry, like `/login`: it
    // has to run before the admission it is meant to satisfy. Kept out of the
    // route table because it answers a CORS preflight as well as the POST.
    // Passwordless, so it exists only where the deployment opted in with
    // `vendor_login = on` (ADR-0032); otherwise the path falls through to the
    // normal admission and the entry does not exist at all.
    if path == "/vendor-login" && config.settings.vendor_login {
        return crate::face::vendor_login::respond(request, lan);
    }
    let auth = match Admission::new(lan).admit(&request.headers) {
        Ok(auth) => auth,
        Err(Rejected::Unavailable) => return error(500, "internal", "LAN state unavailable"),
        Err(Rejected::Unauthorized) => {
            // One redirect projection for both the buffered and the streamed
            // plugin UI face (#219); API clients keep the 401.
            if request.method == "GET" && path.starts_with("/plugin-ui/") {
                return unauthenticated(&request.headers, path);
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
        if request.method == "GET" && ui.slashless && web_assets_for(&config.paths, ui.id).is_some()
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
    let assets = match static_assets(&config.paths, lan, ui) {
        Ok(assets) => assets,
        Err(response) => return response,
    };
    if ui.sub == "rpc" {
        return ui_rpc(config.paths.control_dir(), request, ui.id, auth);
    }
    let base = format!("/plugin-ui/{}/{}/", ui.id, ui.token);
    serve_ui(request, &assets, ui.sub, &base)
}

fn serve_ui(request: &HttpRequest, assets: &WebAssets, sub: &str, base: &str) -> Vec<u8> {
    if request.method != "GET" {
        return method_not_allowed("GET");
    }
    let Some((path, metadata)) = webui::resolve_asset(assets, sub, &request.headers) else {
        return error(404, "not_found", "not found");
    };
    serve_web_file(request, &path, &metadata, base)
}

pub(crate) fn serve_web_file(
    request: &HttpRequest,
    path: &Path,
    metadata: &fs::Metadata,
    base: &str,
) -> Vec<u8> {
    let mime = http::content_type(&path.to_string_lossy());
    let validators = webui::asset_validators(metadata, request_header(request, "if-none-match"));
    if validators.matches_if_none_match() {
        return http::response_with_headers(304, mime, validators.headers(), b"");
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
    http::response_with_headers(200, mime, validators.headers(), &body)
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
    let matched = match routes::resolve(&request.method, &request.path) {
        routes::Match::Found(matched) if matched.spec.owner == Owner::Fallback => matched,
        routes::Match::MethodNotAllowed { allow, .. } => return method_not_allowed(&allow),
        _ => return error(404, "not_found", "unknown endpoint"),
    };
    let param = |name: &str| decode(matched.param(name).unwrap_or(""));
    match matched.spec.endpoint {
        Endpoint::BlobCreate => blob_routes::create_upload(&config.paths, blob::UploadKind::Blob),
        Endpoint::BlobStatus => {
            blob_routes::upload_status(&config.paths, blob::UploadKind::Blob, &param("id"))
        }
        Endpoint::BlobComplete => {
            blob_routes::complete_upload(&config.paths, &param("id"), &request.body)
        }
        Endpoint::BlobAbort => {
            blob_routes::abort_upload(&config.paths, blob::UploadKind::Blob, &param("id"))
        }
        Endpoint::FileCreate => blob_routes::create_upload(&config.paths, blob::UploadKind::File),
        Endpoint::FileStatus => {
            blob_routes::upload_status(&config.paths, blob::UploadKind::File, &param("id"))
        }
        Endpoint::FileComplete => {
            blob_routes::file_upload_complete(&config.paths, &param("id"), &request.body)
        }
        Endpoint::FileAbort => {
            blob_routes::abort_upload(&config.paths, blob::UploadKind::File, &param("id"))
        }
        _ => error(404, "not_found", "unknown endpoint"),
    }
}

fn method_not_allowed(allow: &str) -> Vec<u8> {
    http::method_not_allowed(allow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn request(if_none_match: Option<&str>) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            path: "/plugin-ui/demo/tok/app.js".into(),
            version: "HTTP/1.1".into(),
            headers: if_none_match
                .map(|value| vec![("If-None-Match".to_string(), value.to_string())])
                .unwrap_or_default(),
            body: Vec::new(),
        }
    }

    #[test]
    fn an_uppercase_html_accept_redirects_from_the_fallback_face() {
        let config = Config::with_paths(
            crate::platform::settings::Settings::default(),
            None,
            crate::platform::paths::Paths::under(&crate::platform::test_support::temp_dir()),
        );
        let lan = Arc::new(Mutex::new(Lan::new(
            "127.0.0.1:0".parse().expect("addr"),
            "127.0.0.1:0".parse().expect("addr"),
            true,
            Some("not-a-real-hash".into()),
        )));
        let request = HttpRequest {
            method: "GET".into(),
            path: "/plugin-ui/demo/".into(),
            version: "HTTP/1.1".into(),
            headers: vec![("Accept".into(), "TEXT/HTML".into())],
            body: Vec::new(),
        };

        let response = respond(&config, &request, &lan);

        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 303 See Other"), "{text}");
        assert!(
            text.contains("Location: /?next=/plugin-ui/demo/\r\n"),
            "{text}"
        );
    }

    /// `/vendor-login` is passwordless, so it only exists behind the
    /// `vendor_login = on` opt-in; a stock daemon keeps normal admission on the
    /// same path (a 401, not a special entry).
    #[test]
    fn the_vendor_handoff_requires_the_opt_in_key() {
        let root = crate::platform::test_support::temp_dir().join(format!(
            "svcd-fallback-vendor-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("config")).expect("temp config");
        let lan = Arc::new(Mutex::new(Lan::new(
            "127.0.0.1:0".parse().expect("addr"),
            "127.0.0.1:0".parse().expect("addr"),
            true,
            Some(crate::platform::auth::hash_password("secret").expect("hash")),
        )));
        let request = HttpRequest {
            method: "POST".into(),
            path: "/vendor-login".into(),
            version: "HTTP/1.1".into(),
            headers: vec![
                ("Host".into(), "192.168.5.1:8787".into()),
                ("Origin".into(), "http://192.168.5.1".into()),
                ("X-Vendor-OK".into(), "1".into()),
            ],
            body: Vec::new(),
        };
        let paths = crate::platform::paths::Paths::under(&root);

        let closed = Config::with_paths(
            crate::platform::settings::Settings::default(),
            None,
            paths.clone(),
        );
        let closed = String::from_utf8_lossy(&respond(&closed, &request, &lan)).to_string();
        assert!(closed.starts_with("HTTP/1.1 401"), "{closed}");

        let mut settings = crate::platform::settings::Settings::default();
        settings.vendor_login = true;
        let open = Config::with_paths(settings, None, paths);
        let open = String::from_utf8_lossy(&respond(&open, &request, &lan)).to_string();
        assert!(open.starts_with("HTTP/1.1 200"), "{open}");
        assert!(open.contains("Set-Cookie: svcd_session="), "{open}");
    }

    /// The buffered face answers 304 from the same projection the streamed
    /// face uses, and both paths carry the projection's whole header set.
    #[test]
    fn the_buffered_asset_path_answers_304_with_the_shared_projection_headers() {
        let root = crate::platform::test_support::temp_dir().join(format!(
            "svcd-fallback-asset-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temp root");
        fs::write(root.join("app.js"), b"console.log(1);").expect("write asset");
        let (path, metadata) = webui::web_file(&root, "app.js").expect("asset");
        let tag = webui::etag(metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len());
        let headers: Vec<String> = webui::asset_validators(&metadata, None)
            .headers()
            .iter()
            .map(|(name, value)| format!("{}: {}\r\n", name, value))
            .collect();

        let fresh = serve_web_file(&request(Some(&format!("W/{}", tag))), &path, &metadata, "/");
        let fresh = String::from_utf8_lossy(&fresh).to_string();
        assert!(
            fresh.starts_with("HTTP/1.1 304 Not Modified\r\n"),
            "{fresh}"
        );
        for header in &headers {
            assert!(fresh.contains(header), "304 lost {header:?}:\n{fresh}");
        }

        let body = serve_web_file(&request(None), &path, &metadata, "/");
        let body = String::from_utf8_lossy(&body).to_string();
        assert!(body.starts_with("HTTP/1.1 200 OK\r\n"), "{body}");
        for header in &headers {
            assert!(body.contains(header), "200 lost {header:?}:\n{body}");
        }

        let _ = fs::remove_dir_all(&root);
    }
}
