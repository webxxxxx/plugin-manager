//! Plugin UI realtime routes as sans-IO stream handlers.
//!
//! SSE, the page WebSocket, and the protocol-3 raw WebSocket all used to own a
//! dedicated state machine in the legacy HTTP reactor. They are handlers now:
//! the server owns the socket, the output buffer and the deadline; a handler
//! only turns events into bytes and asks for a blocking job when it must wait
//! on the control channel.
//!
//! Endpoint policy stays here (plugin token, Origin/CSRF, handshake, raw
//! availability); the server resolved the transport principal before the face
//! ran.

mod frame;
pub mod raw;
pub mod sse;
pub mod ws;

use std::sync::{Arc, Mutex};

use crate::face::plugin_ui::{PluginUiStream, UiPath};
use crate::face::uichannel;
use crate::manage::package;
use crate::manage::plugin;
use crate::platform::paths::Paths;
use crate::server::auth::lan::{AuthKind, Lan};
use crate::server::http::model as http;
use crate::server::http::{Request, StreamHandler};

/// Everything the realtime handlers need that is not the request itself. The
/// legacy engine always has it; a build without a raw transport has `None`.
pub(crate) struct StreamContext {
    /// The one directory set: the control endpoint (`paths.config_dir`) and the
    /// plugin tree both come from it.
    pub paths: Arc<Paths>,
    pub raw_hub: Option<Arc<crate::wire::contract::rawhub::RawHub>>,
    pub limits: Arc<crate::platform::limits::Limits>,
}

/// Why a realtime request was refused before its first handler byte.
type Refusal = Vec<u8>;

fn boxed(response: Vec<u8>) -> Box<dyn StreamHandler> {
    Box::new(PluginUiStream::response(response))
}

fn unknown_ui() -> Refusal {
    http::json_error(404, "not_found", "unknown plugin UI")
}

fn valid_ui(paths: &Paths, ui: &UiPath<'_>, require_assets: bool) -> Result<(), Refusal> {
    if !package::valid_name(ui.id) {
        return Err(unknown_ui());
    }
    if require_assets && plugin::web_assets(paths, ui.id).is_none() {
        return Err(unknown_ui());
    }
    Ok(())
}

fn valid_token(lan: &Arc<Mutex<Lan>>, ui: &UiPath<'_>) -> Result<(), Refusal> {
    if crate::server::auth::admission::plugin_token_valid(lan, ui.token, ui.id) {
        Ok(())
    } else {
        Err(http::json_error(
            401,
            "unauthorized",
            "invalid or expired plugin token",
        ))
    }
}

fn same_origin(headers: &[(String, String)]) -> Result<(), Refusal> {
    if http::header(headers, "origin").is_some() && !http::same_origin(headers) {
        return Err(http::json_error(
            403,
            "forbidden",
            "cross-origin calls are not allowed",
        ));
    }
    Ok(())
}

fn cookie_origin(
    principal: AuthKind,
    headers: &[(String, String)],
    message: &str,
) -> Result<(), Refusal> {
    if principal == AuthKind::Cookie && http::header(headers, "origin").is_none() {
        return Err(http::json_error(403, "forbidden", message));
    }
    Ok(())
}

fn handshake(headers: &[(String, String)]) -> Result<String, Refusal> {
    uichannel::ws_accept(headers).map_err(|message| {
        http::response_with_headers(
            426,
            "text/plain; charset=utf-8",
            &[
                ("Upgrade".to_string(), "websocket".to_string()),
                ("Sec-WebSocket-Version".to_string(), "13".to_string()),
            ],
            message.as_bytes(),
        )
    })
}

fn too_many_subscribers() -> Refusal {
    http::json_error(
        429,
        "too_many_subscribers",
        "too many UI subscribers for this plugin",
    )
}

/// `/plugin-ui/{id}/{token}/events`: subscribe before returning the handler so
/// a replay ring failure is a normal HTTP error, not a stream that dies later.
pub(crate) fn open_sse(
    ui: &UiPath<'_>,
    request: &Request,
    lan: &Arc<Mutex<Lan>>,
    context: &StreamContext,
) -> Box<dyn StreamHandler> {
    if let Err(response) = valid_ui(&context.paths, ui, true) {
        return boxed(response);
    }
    if let Err(response) = valid_token(lan, ui) {
        return boxed(response);
    }
    if let Err(response) = same_origin(&request.headers) {
        return boxed(response);
    }
    let last_event_id = http::header(&request.headers, "last-event-id")
        .and_then(|value| value.trim().parse::<u64>().ok());
    match uichannel::bus().subscribe_from(ui.id, last_event_id, &context.limits) {
        Ok((receiver, subscription, replay)) => {
            Box::new(sse::SseStream::new(receiver, subscription, replay))
        }
        Err(_) => boxed(too_many_subscribers()),
    }
}

/// `/plugin-ui/{id}/{token}/ws`: the page WebSocket. The RPC bridge still
/// answers through `registry.call`; the handler submits those round trips as
/// blocking jobs instead of running them on the server loop.
pub(crate) fn open_ws(
    ui: &UiPath<'_>,
    request: &Request,
    lan: &Arc<Mutex<Lan>>,
    context: &StreamContext,
) -> Box<dyn StreamHandler> {
    if let Err(response) = valid_ui(&context.paths, ui, true) {
        return boxed(response);
    }
    if let Err(response) = valid_token(lan, ui) {
        return boxed(response);
    }
    if let Err(response) = same_origin(&request.headers) {
        return boxed(response);
    }
    if let Err(response) = cookie_origin(
        request.principal,
        &request.headers,
        "Origin is required for cookie-authenticated WebSocket calls",
    ) {
        return boxed(response);
    }
    let accept = match handshake(&request.headers) {
        Ok(accept) => accept,
        Err(response) => return boxed(response),
    };
    match uichannel::bus().subscribe(ui.id, &context.limits) {
        Ok((receiver, subscription)) => Box::new(ws::WsStream::new(
            context.paths.config_dir.clone(),
            ui.id.to_string(),
            accept,
            receiver,
            subscription,
            context.limits.clone(),
        )),
        Err(_) => boxed(too_many_subscribers()),
    }
}

/// `/plugin-ui/{id}/{token}/raw`: the protocol-3 binary transport. It does not
/// require a web UI: a headless plugin may still expose raw frames.
pub(crate) fn open_raw(
    ui: &UiPath<'_>,
    request: &Request,
    lan: &Arc<Mutex<Lan>>,
    context: &StreamContext,
) -> Box<dyn StreamHandler> {
    if let Err(response) = valid_ui(&context.paths, ui, false) {
        return boxed(response);
    }
    if !context.limits.ws_raw_enabled {
        return boxed(http::json_error(
            403,
            "forbidden",
            "raw transport is disabled",
        ));
    }
    if let Err(response) = valid_token(lan, ui) {
        return boxed(response);
    }
    if let Err(response) = same_origin(&request.headers) {
        return boxed(response);
    }
    if let Err(response) = cookie_origin(
        request.principal,
        &request.headers,
        "Origin is required for cookie-authenticated calls",
    ) {
        return boxed(response);
    }
    let accept = match handshake(&request.headers) {
        Ok(accept) => accept,
        Err(response) => return boxed(response),
    };
    let Some(hub) = &context.raw_hub else {
        return boxed(http::json_error(
            501,
            "not_supported",
            "raw transport is not supported on this platform",
        ));
    };
    match hub.subscribe(ui.id) {
        Ok(subscription) => Box::new(raw::RawWsStream::new(
            accept,
            Box::new(subscription),
            Some(hub.clone()),
            context.limits.clone(),
        )),
        Err(message) => boxed(http::json_error(
            503,
            "raw_unavailable",
            &format!("raw transport unavailable: {}", message),
        )),
    }
}
