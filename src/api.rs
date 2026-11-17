//! Runtime assembly, authentication, and the transfer/MCP paths that are not
//! yet faces.
//!
//! `bind` in `svcd.conf` chooses the listener address. A non-loopback bind
//! requires `auth_passwd`; without it the daemon falls back to loopback so the
//! operator can still run `svc lan passwd`. Management JSON projections and
//! the stream endpoints live in `face/` (#117–#121); the connection drivers
//! (the Unix poll loop and the per-connection threads) live in `server/http/`,
//! which is also where the platform choice is made (#139). This module is the
//! composition root plus the endpoints the face does not own yet.

use std::fs;
use std::io::Write;
use std::net::TcpStream;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::json;

use crate::blob;
use crate::face::blob_routes;
use crate::face::support::{decode, error};
use crate::channel;
use crate::face::plugin_ui::{ui_path, UiPath};
use crate::http;
use crate::lan::{self, AuthKind, Lan, LanStatus};
use crate::log;
use crate::server::mcp::transport;
use crate::package;
use crate::plugin;
use crate::rpc;
use crate::server::http::threads::{
    client_gone, drive_fallback, read_body, read_head, write_http, HeadFailure,
};
use crate::server::http::engine::HttpRequest;
use crate::server::http::engine::{
    keep_alive_response, wants_keep_alive, wants_keep_alive_head,
};
use crate::settings;
use crate::threads;
use crate::webui;

pub struct Api {
    lan: Arc<Mutex<Lan>>,
    /// Stops this host's connection driver on shutdown.
    driver: crate::server::http::entry::ServerHandle,
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}

fn resolve_bind(env: Option<&str>, configured: Option<SocketAddr>) -> SocketAddr {
    env.and_then(|value| value.parse::<SocketAddr>().ok())
        .or(configured)
        .unwrap_or_else(default_bind)
}

/// Compose the HTTP surface and start this host's connection driver.
///
/// `control` is the bound control channel; a poll-loop host consumes its
/// listener, a thread-per-connection host ignores it (the channel already
/// started its accept thread). Which driver runs is decided in
/// `server::http::entry`, not here.
pub fn spawn(
    config: PathBuf,
    raw_hub: Arc<crate::rawhub::RawHub>,
    control: crate::channel::ControlBinding,
) -> std::io::Result<Arc<Api>> {
    let configured = settings::current().ok().and_then(|settings| settings.bind);
    let requested = resolve_bind(std::env::var("SVCD_BIND").ok().as_deref(), configured);
    let password_hash = settings::current()
        .ok()
        .and_then(|settings| settings.auth_passwd.clone());

    // A LAN bind without a password would expose an unauthenticated API. Keep
    // the daemon reachable on loopback so the operator can still run
    // `svc lan passwd`, then restart to pick up the requested bind.
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
    // A failed bind disables HTTP only: the Unix loop still serves the CLI,
    // and the thread driver reports the same failure through `entry::start`.
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
    let routes: Arc<dyn crate::server::http::hooks::PreBodyRoutes> = Arc::new(PreBodyFaces);
    let fallback: crate::server::http::hooks::FallbackHandler =
        Arc::new(move |request, lan| respond(&fallback_config, request, lan));

    // The thread driver: one thread per accepted socket. It only runs on hosts
    // without a poll loop, but the closure compiles everywhere so this file
    // keeps no platform branch.
    let connection: crate::server::http::threads::Connection =
        Arc::new(|stream, peer, config, lan, idle_timeout| {
            handle(stream, peer, config, lan, idle_timeout)
        });
    let driver_config = config.clone();
    let threads: crate::server::http::entry::ThreadDriver = Arc::new(move |listener, lan, _| {
        let limits = crate::limits::current();
        let connection = connection.clone();
        let admission = crate::server::http::threads::admission(
            limits.max_connections,
            limits.max_connections_per_ip,
        );
        let idle_timeout = limits.idle_timeout;
        let stop = Arc::new(AtomicBool::new(false));
        let listener_stop = stop.clone();
        let serve_config = driver_config.clone();
        let spawned = thread::Builder::new()
            .name("svcd-http".into())
            .stack_size(threads::io_stack())
            .spawn(move || {
                crate::server::http::threads::serve(
                    listener,
                    serve_config,
                    lan,
                    admission,
                    idle_timeout,
                    listener_stop,
                    connection,
                )
            });
        if let Err(error) = spawned {
            log::svcd(&format!("svcd: HTTP listener thread failed: {}", error));
        }
        crate::server::http::entry::ServerHandle::thread(stop)
    });

    let driver = crate::server::http::entry::start(crate::server::http::entry::StartInput {
        config,
        lan: lan.clone(),
        raw_hub,
        listener,
        face,
        routes,
        fallback,
        control,
        threads,
    })?;
    Ok(Arc::new(Api { lan, driver }))
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
        self.driver.shutdown();
    }
}


/// Run one plugin UI request on the Windows connection thread. The face
/// resolves the endpoint policy; the server layer's fallback driver owns the
/// socket and the handler's events.
fn serve_plugin_ui(
    stream: &mut TcpStream,
    head: &http::Head,
    peer: IpAddr,
    lan: &Arc<Mutex<Lan>>,
    config: PathBuf,
) {
    use crate::server::http::Exchange;

    let head_only = http::is_head(&head.method);
    match crate::face::plugin_ui::PluginUiFace::exchange(head, peer, lan, config, None) {
        Err(response) => {
            let _ = write_http(stream, &response, head_only);
        }
        Ok(Exchange::Response(response)) => {
            let _ = write_http(stream, &response.into_bytes(), head_only);
        }
        Ok(Exchange::Stream(handler)) => drive_fallback(stream, handler, head_only),
    }
}

/// The composition root's pre-body stream faces. The server loop holds this
/// trait object instead of depending on the face layer directly.
struct PreBodyFaces;

impl crate::server::http::hooks::PreBodyRoutes for PreBodyFaces {
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
fn serve_package_upload(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
    config: PathBuf,
) {
    use crate::server::http::Exchange;

    let head_only = http::is_head(&head.method);
    match crate::face::install::exchange(head, lan, config) {
        Err(response) => {
            let _ = write_http(stream, &response, head_only);
        }
        Ok(Exchange::Response(_)) => {
            // A streamed route must not answer with a memory response; the
            // Unix loop renders the same 500 before any handler bytes go out.
            let _ = write_http(
                stream,
                &http::json_error(
                    500,
                    "internal",
                    "a streamed package upload route returned a memory response",
                ),
                head_only,
            );
        }
        Ok(Exchange::Stream(handler)) => {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(crate::limits::current().transfer_timeout));
            let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
            if crate::server::http::fallback::drive(stream, handler, initial).is_err() {
                let _ = write_http(
                    stream,
                    &crate::server::http::stream_busy_response(),
                    head_only,
                );
            }
        }
    }
}

/// Run one blob/file upload through the face and the Windows fallback driver.
/// The fallback owns the socket and runs upload staging jobs on this
/// connection thread, so a slow client only blocks its own connection.
fn serve_blob_upload(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
) {
    use crate::server::http::Exchange;

    let head_only = http::is_head(&head.method);
    match crate::face::blob_upload::exchange(head, lan) {
        Err(response) => {
            let _ = write_http(stream, &response, head_only);
        }
        Ok(Exchange::Response(response)) => {
            let _ = write_http(stream, &response.into_bytes(), head_only);
        }
        Ok(Exchange::Stream(handler)) => {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(crate::limits::current().transfer_timeout));
            let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
            if crate::server::http::fallback::drive(stream, handler, initial).is_err() {
                let _ = write_http(
                    stream,
                    &crate::server::http::stream_busy_response(),
                    head_only,
                );
            }
        }
    }
}

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
        let head_only = http::is_head(&head.method);
        // Resolve the face route and the principal exactly once, before any
        // body byte is read. The route's BodyPolicy is the only body limit.
        let planned = match crate::server::http::route_face_request(&face, &head, peer, &lan) {
            Ok(planned) => planned,
            Err(response) => {
                // A bodyless request can keep the connection alive after a
                // refusal; a request that declared a body is closed so its
                // unread bytes cannot precede the next request.
                let empty = matches!(
                    http::framing(&head.headers),
                    Ok(http::Framing::None) | Ok(http::Framing::Length(0))
                );
                let keep_alive = empty && wants_keep_alive_head(&head);
                let response = if keep_alive {
                    keep_alive_response(response)
                } else {
                    response
                };
                let _ = write_http(&mut stream, &response, head_only);
                if !keep_alive {
                    return;
                }
                continue;
            }
        };
        if let Some(prepared) = planned {
            let keep_alive = wants_keep_alive_head(&head);
            let body = match read_body(&mut stream, &head, &initial, prepared.route.body) {
                Ok(body) => body,
                Err(response) => {
                    let _ = write_http(&mut stream, &response, head_only);
                    return;
                }
            };
            let prepared = match crate::server::http::attach_face_body(prepared, &body) {
                Ok(prepared) => prepared,
                Err(response) => {
                    let _ = write_http(&mut stream, &response, head_only);
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
                    drive_fallback(&mut stream, handler, head_only);
                    return;
                }
                result => {
                    let mut response = crate::server::http::render_exchange(result);
                    if keep_alive {
                        response = keep_alive_response(response);
                    }
                    if write_http(&mut stream, &response, head_only).is_err() {
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
                let _ = write_http(&mut stream, &response, head_only);
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
            match transport::prepare_streaming(
                &transport::Request {
                    method: &request.method,
                    headers: &request.headers,
                    body: &request.body,
                },
                password_hash.as_deref(),
            ) {
                Ok(Some(call)) => {
                    let _ = serve_mcp_sse(&mut stream, call, &config, head_only);
                    return;
                }
                Ok(None) => {}
                Err(response) => {
                    let _ = write_http(&mut stream, &response, head_only);
                    return;
                }
            }
        }
        let mut response = respond(&config, &request, &lan);
        if keep_alive {
            response = keep_alive_response(response);
        }
        if write_http(&mut stream, &response, head_only).is_err() {
            return;
        }
        if !keep_alive {
            return;
        }
    }
}

fn serve_mcp_sse(
    stream: &mut TcpStream,
    call: transport::StreamingCall,
    config: &Path,
    head_only: bool,
) -> std::io::Result<()> {
    // The Windows fallback has no stream driver, but the long-lived budget is
    // still server policy: reserve it before the head and before the plugin
    // call starts. The permit lives until this blocking response ends, and a
    // client disconnect ends it below (#135).
    let _permit = match crate::server::http::budget::StreamBudget::global().acquire(
        crate::server::http::StreamClass::LongLived,
        crate::limits::current().max_streams,
    ) {
        Ok(permit) => permit,
        Err(_) => {
            write_http(
                stream,
                &crate::server::http::stream_busy_response(),
                head_only,
            )?;
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
    // The control read runs on this connection's thread, so it must be able to
    // see the client leave. A cloned handle gives the poll a read-only probe
    // while the response keeps writing to the original socket.
    let probe = match stream.try_clone() {
        Ok(probe) => probe,
        Err(_) => {
            let response = rpc::err(call.id, -32603, "cannot observe the client connection");
            let _ = write_sse_line(stream, &rpc::to_line(&response));
            let _ = stream.write_all(b"0\r\n\r\n");
            let _ = stream.flush();
            return Ok(());
        }
    };
    // `abandoned` means the SSE response can no longer be delivered: the
    // client left or a write already failed. The poll closure sets it and the
    // notification sink does the same, so neither path has to inspect an
    // `io::ErrorKind` to know the connection is gone.
    let abandoned = Arc::new(AtomicBool::new(false));
    let left = abandoned.clone();
    let polled = abandoned.clone();
    let response = channel::request_stream(
        config,
        "registry.call",
        json!({
            "name": call.name,
            "arguments": call.arguments,
            "progressToken": call.progress_token,
        }),
        // The same no-progress bound the Unix stream driver applies to an SSE
        // response: a hung supervisor cannot pin this thread forever.
        Some(crate::limits::current().transfer_timeout),
        move || {
            let gone = left.load(Ordering::Acquire) || client_gone(&probe);
            if gone {
                polled.store(true, Ordering::Release);
            }
            gone
        },
        |notification| {
            if !abandoned.load(Ordering::Acquire)
                && write_sse_line(stream, notification).is_err()
            {
                abandoned.store(true, Ordering::Release);
            }
        },
    );
    let response = match response {
        Ok(response) => Some(crate::server::mcp::result::finish_modern_tool_call(
            call.id,
            response,
        )),
        // The client left, or a write failed: this response can no longer be
        // delivered. Dropping the control socket discards a late final frame
        // instead of pooling it for the next request.
        Err(_) if abandoned.load(Ordering::Acquire) => None,
        Err(error) => Some(rpc::err(
            call.id,
            -32603,
            &format!("svcd unreachable: {}", error),
        )),
    };
    if !abandoned.load(Ordering::Acquire) {
        if let Some(response) = &response {
            let _ = write_sse_line(stream, &rpc::to_line(response));
        }
        let _ = stream.write_all(b"0\r\n\r\n");
        let _ = stream.flush();
    }
    Ok(())
}

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
        return transport::handle(
            &|request| crate::mcp::serve_modern(config, request),
            &transport::Request {
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

fn csrf_ok(request: &HttpRequest) -> bool {
    http::csrf_ok(&request.headers)
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
            blob_routes::create_upload(blob::UploadKind::Blob)
        }
        ("GET", ["api", "v1", "blobs", "uploads", id]) => {
            blob_routes::upload_status(blob::UploadKind::Blob, &decode(id))
        }
        ("POST", ["api", "v1", "blobs", "uploads", id, "complete"]) => {
            blob_routes::complete_upload(&decode(id), &request.body)
        }
        ("DELETE", ["api", "v1", "blobs", "uploads", id]) => {
            blob_routes::abort_upload(blob::UploadKind::Blob, &decode(id))
        }
        (_, ["api", "v1", "blobs"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads", _]) => method_not_allowed("GET, DELETE"),
        (_, ["api", "v1", "blobs", "uploads", _, "complete"]) => method_not_allowed("POST"),
        (_, ["api", "v1", "blobs", "uploads", _, "parts", _]) => method_not_allowed("PUT"),
        (_, ["api", "v1", "blobs", _]) => method_not_allowed("GET"),
        ("POST", ["api", "v1", "file", "uploads"]) => {
            blob_routes::create_upload(blob::UploadKind::File)
        }
        ("GET", ["api", "v1", "file", "uploads", id]) => {
            blob_routes::upload_status(blob::UploadKind::File, &decode(id))
        }
        ("POST", ["api", "v1", "file", "uploads", id, "complete"]) => {
            blob_routes::file_upload_complete(&decode(id), &request.body)
        }
        ("DELETE", ["api", "v1", "file", "uploads", id]) => {
            blob_routes::abort_upload(blob::UploadKind::File, &decode(id))
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

}
