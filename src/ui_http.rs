//! Streaming plugin-UI static assets.
//!
//! Static assets under `/plugin-ui/{id}/{token}/...` are read from disk with
//! `sendfile` instead of `fs::read`. HTML continues through `inject_base`, and
//! `/rpc`, SSE and WebSocket paths stay on the normal router.

use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::api;
use crate::http;
use crate::lan::Lan;
use crate::package;
use crate::plugin;
use crate::webui;

pub struct Route<'a> {
    path: &'a str,
    ui: api::UiPath<'a>,
}

pub fn route(head: &http::Head) -> Option<Route<'_>> {
    if head.method != "GET" {
        return None;
    }
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(head.path.as_str());
    let ui = api::ui_path(path)?;
    if ui.slashless || matches!(ui.sub, "rpc" | "events" | "ws") {
        return None;
    }
    Some(Route { path, ui })
}

pub fn serve(stream: &mut TcpStream, head: &http::Head, lan: &Arc<Mutex<Lan>>) {
    let Some(route) = route(head) else {
        return;
    };
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
    let auth = match lan.lock() {
        Ok(mut state) => state.authenticate(&head.headers),
        Err(_) => Err(()),
    };
    let auth = match auth {
        Ok(auth) => auth,
        Err(()) => {
            if wants_html(head) {
                let _ = stream.write_all(&http::response_with_headers(
                    303,
                    "text/plain; charset=utf-8",
                    &[(
                        "Location".to_string(),
                        format!("/?next={}", api::encode_query_value(route.path)),
                    )],
                    b"",
                ));
            } else {
                let _ = stream.write_all(&http::json_error(
                    401,
                    "unauthorized",
                    "authentication required",
                ));
            }
            return;
        }
    };
    let _ = auth;

    let ui = &route.ui;
    if !package::valid_name(ui.id) {
        let _ = stream.write_all(&http::json_error(404, "not_found", "unknown plugin UI"));
        return;
    }
    let Some(assets) = plugin::web_assets(ui.id) else {
        let _ = stream.write_all(&http::json_error(
            404,
            "not_found",
            &format!("plugin {} has no web UI", ui.id),
        ));
        return;
    };
    if !api::token_valid(lan, ui.token, ui.id) {
        let _ = stream.write_all(&http::json_error(
            401,
            "unauthorized",
            "invalid or expired plugin token",
        ));
        return;
    }
    let Some(relative) = webui::relative_path(ui.sub, &assets.entry) else {
        let _ = stream.write_all(&http::json_error(404, "not_found", "not found"));
        return;
    };
    let base = format!("/plugin-ui/{}/{}/", ui.id, ui.token);
    let file = api::web_file(&assets.root, &relative).or_else(|| {
        if wants_html(head) {
            api::web_file(&assets.root, &assets.entry)
        } else {
            None
        }
    });
    let Some((path, metadata)) = file else {
        let _ = stream.write_all(&http::json_error(404, "not_found", "not found"));
        return;
    };
    let mime = http::content_type(&path.to_string_lossy());
    let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
    let etag = webui::etag(modified, metadata.len());
    let headers = api::web_headers(&etag);
    if webui::not_modified(http::header(&head.headers, "if-none-match"), &etag) {
        let _ = stream.write_all(&http::response_with_headers(304, mime, &headers, b""));
        return;
    }
    if mime.starts_with("text/html") {
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) => {
                let _ = stream.write_all(&http::json_error(
                    500,
                    "internal",
                    &format!("read UI asset: {}", error),
                ));
                return;
            }
        };
        let body = webui::inject_base(&data, &base);
        let _ = stream.write_all(&http::response_with_headers(200, mime, &headers, &body));
        return;
    }
    let length = metadata.len();
    let _ = stream.write_all(&http::response_head(200, mime, &headers, length));
    let mut file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(_) => return,
    };
    if length >= 64 * 1024 {
        if let Ok(true) = crate::zerocopy::send_file_range(&mut file, stream, 0, length) {
            let _ = stream.flush();
            return;
        }
    }
    let _ = io::copy(&mut file, stream);
    let _ = stream.flush();
}

fn wants_html(head: &http::Head) -> bool {
    http::header(&head.headers, "accept")
        .map(|value| value.to_ascii_lowercase().contains("text/html"))
        .unwrap_or(false)
}
