//! Streaming plugin exports.
//!
//! `GET /api/v1/export/{id}` (`.spkb`), `.../{id}/spk` and `.../{id}/config`
//! are generated straight onto the socket. No package bytes are buffered and no
//! temporary archive is written.

use std::io::{self, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::http;
use crate::lan::{AuthKind, Lan};
use crate::plugin;

pub enum Route {
    Spk { id: String },
    Spkb { id: String },
    Config { id: String },
}

pub fn route(head: &http::Head) -> Option<Route> {
    if head.method != "GET" {
        return None;
    }
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(head.path.as_str());
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["api", "v1", "export", id, "spk"] => Some(Route::Spk {
            id: http::percent_decode(id, false),
        }),
        ["api", "v1", "export", id] => Some(Route::Spkb {
            id: http::percent_decode(id, false),
        }),
        ["api", "v1", "export", id, "config"] => Some(Route::Config {
            id: http::percent_decode(id, false),
        }),
        _ => None,
    }
}

pub fn serve(
    stream: &mut TcpStream,
    head: &http::Head,
    lan: &Arc<Mutex<Lan>>,
    config: &Path,
    route: Route,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
    let auth = match lan.lock() {
        Ok(mut state) => state.authenticate(&head.headers),
        Err(_) => Err(()),
    };
    match auth {
        Ok(AuthKind::None) | Ok(AuthKind::Bearer) | Ok(AuthKind::Cookie) => {}
        Err(()) => {
            let _ = stream.write_all(&http::json_error(
                401,
                "unauthorized",
                "authentication required",
            ));
            return;
        }
    }

    let (id, config_only) = match &route {
        Route::Spk { id } => (id.as_str(), false),
        Route::Spkb { id } => (id.as_str(), false),
        Route::Config { id } => (id.as_str(), true),
    };
    if let Err(response) = crate::api::require_plugin(config, id) {
        let _ = stream.write_all(&response);
        return;
    }
    let Some(version) = plugin::current_version(id) else {
        let _ = stream.write_all(&http::json_error(
            404,
            "not_found",
            &format!("unknown plugin: {}", id),
        ));
        return;
    };
    if config_only {
        let Some(path) = plugin::active_config(id) else {
            let _ = stream.write_all(&http::json_error(
                404,
                "not_found",
                &format!("plugin {} has no config", id),
            ));
            return;
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = stream.write_all(&http::json_error(
                    500,
                    "internal",
                    &format!("stat config: {}", error),
                ));
                return;
            }
        };
        let headers = vec![
            (
                "Content-Disposition".to_string(),
                format!(
                    "attachment; filename=\"{}-{}.config\"",
                    safe(id),
                    safe(&version)
                ),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ];
        let _ = stream.write_all(&http::response_head(
            200,
            "application/octet-stream",
            &headers,
            metadata.len(),
        ));
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                let _ = stream.write_all(&http::json_error(
                    500,
                    "internal",
                    &format!("open config: {}", error),
                ));
                return;
            }
        };
        let _ = io::copy(&mut file, stream);
        return;
    }

    let filename = match route {
        Route::Spk { .. } => format!("{}-{}.spk", safe(id), safe(&version)),
        Route::Spkb { .. } => format!("{}-{}.spkb", safe(id), safe(&version)),
        Route::Config { .. } => unreachable!(),
    };
    let size = match route {
        Route::Spk { .. } => plugin::export_spk_size(id),
        Route::Spkb { .. } => plugin::export_spkb_size(id),
        Route::Config { .. } => unreachable!(),
    };
    let size = match size {
        Ok(size) => size,
        Err(message) => {
            let _ = stream.write_all(&http::json_error(500, "internal", &message));
            return;
        }
    };
    let headers = vec![
        (
            "Content-Disposition".to_string(),
            format!("attachment; filename=\"{}\"", filename),
        ),
        ("Cache-Control".to_string(), "no-store".to_string()),
    ];
    let _ = stream.write_all(&http::response_head(
        200,
        "application/octet-stream",
        &headers,
        size,
    ));
    let result = match route {
        Route::Spk { .. } => plugin::export_spk_stream(id, &mut *stream).map(|_| ()),
        Route::Spkb { .. } => plugin::export_spkb_stream(id, &mut *stream).map(|_| ()),
        Route::Config { .. } => unreachable!(),
    };
    let _ = result;
    let _ = stream.flush();
}

fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .collect()
}
