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
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::face::fallback::respond;
use crate::face::prebody::PreBodyFaces;
use crate::platform::config::Config;
use crate::platform::host::threads;
use crate::platform::log;
use crate::platform::paths::Paths;
use crate::server::auth::lan::{Lan, LanStatus};
use crate::server::http::engine::HttpRequest;
use crate::server::http::engine::{keep_alive_response, wants_keep_alive, wants_keep_alive_head};
use crate::server::http::model as http;
use crate::server::http::server_body_policy;
use crate::server::http::threads::{drive_fallback, read_body, read_head, write_http, HeadFailure};
use crate::server::mcp::fallback::serve_mcp_sse;
use crate::server::mcp::transport;

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
    config: Arc<Config>,
    raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    control: crate::wire::control::channel::ControlBinding,
) -> std::io::Result<Arc<Api>> {
    let configured = config.settings.bind;
    let requested = resolve_bind(std::env::var("SVCD_BIND").ok().as_deref(), configured);
    let password_hash = config.settings.auth_passwd.clone();

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
                let port_file = config.paths.config_dir.join("http.port");
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
    let routes: Arc<dyn crate::server::http::hooks::PreBodyRoutes> = Arc::new(PreBodyFaces {
        config: config.clone(),
    });
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
    let thread_limits = config.limits.clone();
    let threads: crate::server::http::entry::ThreadDriver = Arc::new(move |listener, lan, _| {
        let limits = &thread_limits;
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
            .stack_size(threads::io_stack(&thread_limits))
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
    config: Arc<Config>,
) {
    use crate::server::http::Exchange;

    let head_only = http::is_head(&head.method);
    let limits = config.limits.clone();
    match crate::face::plugin_ui::PluginUiFace::exchange(head, peer, lan, config, None) {
        Err(response) => {
            let _ = write_http(stream, &response, head_only);
        }
        Ok(Exchange::Response(response)) => {
            let _ = write_http(stream, &response.into_bytes(), head_only);
        }
        Ok(Exchange::Stream(handler)) => drive_fallback(stream, handler, head_only, &limits),
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
    config: Arc<Config>,
) {
    use crate::server::http::Exchange;

    let head_only = http::is_head(&head.method);
    let limits = config.limits.clone();
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
            let _ = stream.set_read_timeout(Some(limits.transfer_timeout));
            let _ = stream.set_write_timeout(Some(limits.transfer_timeout));
            if crate::server::http::fallback::drive(stream, handler, initial, &limits).is_err() {
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
    paths: Arc<Paths>,
    limits: &crate::platform::limits::Limits,
) {
    use crate::server::http::Exchange;

    let head_only = http::is_head(&head.method);
    match crate::face::blob_upload::exchange(head, lan, paths) {
        Err(response) => {
            let _ = write_http(stream, &response, head_only);
        }
        Ok(Exchange::Response(response)) => {
            let _ = write_http(stream, &response.into_bytes(), head_only);
        }
        Ok(Exchange::Stream(handler)) => {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(limits.transfer_timeout));
            let _ = stream.set_write_timeout(Some(limits.transfer_timeout));
            if crate::server::http::fallback::drive(stream, handler, initial, &limits).is_err() {
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
    config: Arc<Config>,
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
        let (head, initial) = match read_head(&mut stream, config.limits.max_header) {
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
                        config.limits.control_timeout,
                    )
                }
            };
            match result {
                Ok(crate::server::http::Exchange::Stream(handler)) => {
                    // A streamed response owns this connection until it closes.
                    // The fallback writes the shared 503 if the stream budget is
                    // full before any handler bytes go out.
                    drive_fallback(&mut stream, handler, head_only, &config.limits);
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
            serve_blob_upload(
                &mut stream,
                &head,
                &initial,
                &lan,
                config.paths.clone(),
                &config.limits,
            );
            return;
        }
        let body_policy = server_body_policy(&head, &config.limits);
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
                    let _ = serve_mcp_sse(
                        &mut stream,
                        call,
                        &config.paths.config_dir,
                        head_only,
                        &config.limits,
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::auth::lan::AuthKind;
    use crate::server::http::route_face_request;

    #[test]
    fn a_public_face_route_skips_transport_auth() {
        use crate::server::http::Face;

        let bind = "127.0.0.1:0".parse().expect("bind");
        let lan = Arc::new(Mutex::new(Lan::new(bind, bind, true, Some("hash".into()))));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-api-unit-{}-{}",
            std::process::id(),
            nanos
        ));
        let config = Arc::new(Config::with_paths(
            crate::platform::settings::Settings::default(),
            None,
            Paths::under(&root),
        ));
        let face = crate::face::api::ApiFace::new(config, lan.clone());
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
