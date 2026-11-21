//! The thread-per-connection driver: one thread per accepted socket, used by
//! hosts whose server has no poll loop.
//!
//! Moved out of `api.rs` (#139) so the composition root keeps no platform
//! branch: sockets, admission and framing live here, and the routing/faces are
//! injected as a closure. Everything compiles on every host; the blocking
//! write/disconnect helpers moved to `server/http/blocking.rs` so the MCP
//! fallback can share them without a module cycle.

#![allow(dead_code)] // unused on hosts with a poll loop

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::platform::config::Config;
use crate::platform::host::threads as host_threads;
use crate::platform::log;
use crate::server::auth::lan::Lan;
use crate::server::http::blocking::write_http;
use crate::server::http::engine;
use crate::server::http::engine::{
    keep_alive_response, wants_keep_alive, wants_keep_alive_head, HttpRequest,
};
use crate::server::http::hooks::{FallbackHandler, PreBodyClaim, PreBodyRoutes};
use crate::server::http::model as http;
use crate::server::http::Exchange;
use crate::wire::contract::rawhub::RawHub;

/// Admission bookkeeping for the Windows thread-per-connection fallback.
///
/// The Unix reactor bounds its connection table directly. The fallback has no
/// such table: each accepted socket becomes a thread, so both caps have to be
/// applied *before* that thread exists. One `ConnectionLease` is exactly one
/// admitted connection and exactly one connection thread; dropping the lease
/// when the handler returns releases the global and the per-IP slot together.
///
/// Compiled in test builds on every platform so the policy can be tested
/// without a socket.
pub(crate) struct Admission {
    max_connections: usize,
    max_connections_per_ip: usize,
    active: usize,
    per_ip: HashMap<IpAddr, usize>,
}
struct ConnectionLease {
    admission: Arc<Mutex<Admission>>,
    peer: IpAddr,
}
impl Admission {
    fn new(max_connections: usize, max_connections_per_ip: usize) -> Self {
        Self {
            max_connections,
            max_connections_per_ip,
            active: 0,
            per_ip: HashMap::new(),
        }
    }

    fn lock(admission: &Arc<Mutex<Admission>>) -> std::sync::MutexGuard<'_, Admission> {
        admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
impl ConnectionLease {
    /// Reserve one connection slot for `peer`, or return `None` when either
    /// cap is already reached. A rejected connection must not consume a slot.
    fn acquire(admission: &Arc<Mutex<Admission>>, peer: IpAddr) -> Option<ConnectionLease> {
        let mut state = Admission::lock(admission);
        if state.active >= state.max_connections {
            return None;
        }
        let per_ip = state.per_ip.get(&peer).copied().unwrap_or(0);
        if per_ip >= state.max_connections_per_ip {
            return None;
        }
        state.active += 1;
        *state.per_ip.entry(peer).or_insert(0) += 1;
        Some(ConnectionLease {
            admission: Arc::clone(admission),
            peer,
        })
    }
}
impl Drop for ConnectionLease {
    fn drop(&mut self) {
        let mut state = Admission::lock(&self.admission);
        debug_assert!(state.active > 0, "connection lease dropped without a slot");
        state.active = state.active.saturating_sub(1);
        let remove = match state.per_ip.get_mut(&self.peer) {
            Some(count) => {
                debug_assert!(*count > 0, "connection lease dropped without a per-IP slot");
                *count = count.saturating_sub(1);
                *count == 0
            }
            None => false,
        };
        if remove {
            state.per_ip.remove(&self.peer);
        }
    }
}

/// Everything one connection needs from above this layer: the face, the
/// pre-body route table, the fallback and the raw hub.
///
/// The loop itself stays in this module, so a driver never names a face and the
/// composition root only injects data (#205).
pub(crate) struct Connection {
    pub(crate) face: Arc<dyn crate::server::http::Face>,
    pub(crate) routes: Arc<dyn PreBodyRoutes>,
    pub(crate) fallback: FallbackHandler,
    pub(crate) budget: crate::server::http::budget::StreamBudget,
    pub(crate) raw_hub: Arc<RawHub>,
}

/// The bounded admission caps for this driver: one lease is one connection and
/// one thread, so both caps are applied before the thread exists.
pub(crate) fn admission(
    max_connections: usize,
    max_connections_per_ip: usize,
) -> Arc<Mutex<Admission>> {
    Arc::new(Mutex::new(Admission::new(
        max_connections,
        max_connections_per_ip,
    )))
}

/// Accept loop for the thread driver. `connection` receives the accepted
/// socket and the per-connection settings.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve(
    active: TcpListener,
    config: Arc<Config>,
    lan: Arc<Mutex<Lan>>,
    admission: Arc<Mutex<Admission>>,
    idle_timeout: Duration,
    stop: Arc<AtomicBool>,
    connection: Arc<Connection>,
) {
    let limits = config.limits.clone();
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match active.accept() {
            Ok((stream, peer)) => {
                let ip = peer.ip();
                // Admit before spawning: a refused connection must not create
                // a thread, and an admitted one stays accounted for until its
                // handler returns and drops the lease.
                let Some(lease) = ConnectionLease::acquire(&admission, ip) else {
                    reject_connection(stream);
                    continue;
                };
                let lan = lan.clone();
                let config = config.clone();
                let connection = connection.clone();
                let spawned = thread::Builder::new()
                    .name("svcd-http-conn".into())
                    .stack_size(host_threads::transfer_stack(&limits))
                    .spawn(move || {
                        let _lease = lease;
                        connection.handle(stream, ip, config, lan, idle_timeout);
                    });
                if let Err(error) = spawned {
                    // The failed spawn drops the closure, and with it the
                    // lease, so the slot is released even though no handler
                    // ever ran.
                    log::svcd(&format!("svcd: HTTP connection thread failed: {}", error));
                }
            }
            Err(error) => {
                log::svcd(&format!("svcd: HTTP accept failed: {}", error));
                return;
            }
        }
    }
}
impl Connection {
    /// Serve one accepted socket until it closes or the idle timeout elapses.
    ///
    /// This is the loop that used to sit in `api.rs` (#205): every decision it
    /// takes goes through an injected seam (`Face`, `PreBodyRoutes`,
    /// `FallbackHandler`), so this module names no face and the composition
    /// root only supplies data.
    fn handle(
        &self,
        mut stream: TcpStream,
        peer: IpAddr,
        config: Arc<Config>,
        lan: Arc<Mutex<Lan>>,
        idle_timeout: Duration,
    ) {
        // Windows accepted sockets inherit the listener's non-blocking mode
        // (Unix ones do not). Requests are read with blocking reads, so flip
        // the socket back before installing the read timeout. Otherwise a body
        // that has not arrived yet becomes a WouldBlock "read" error.
        if let Err(error) = stream.set_nonblocking(false) {
            log::svcd(&format!(
                "svcd: HTTP connection: cannot switch to blocking mode: {}",
                error
            ));
        }
        // The configured `http_idle_timeout_secs` replaces the old hard-coded
        // 60 s. It covers any wait for request bytes: the first request, the
        // gap between keep-alive requests, and a stalled request body. That
        // matches the Unix reactor's idle sweep. The same deadline bounds a
        // blocking write, so a client that stops reading cannot pin this
        // connection thread forever; stream handlers below install their own
        // transfer timeout where a longer transfer is expected.
        let _ = stream.set_read_timeout(Some(idle_timeout));
        let _ = stream.set_write_timeout(Some(idle_timeout));
        let face = self.face.as_ref();
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
            // Resolve the face route and the principal exactly once, before
            // any body byte is read. The route's BodyPolicy is the only body
            // limit.
            let planned = match crate::server::http::route_face_request(face, &head, peer, &lan) {
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
                    crate::server::http::Dispatch::Inline => crate::server::http::dispatch_inline(
                        face,
                        &prepared.route,
                        prepared.request,
                    ),
                    crate::server::http::Dispatch::Blocking => {
                        crate::server::http::dispatch_blocking_now(
                            face,
                            &prepared.route,
                            prepared.request,
                            config.limits.control_timeout,
                        )
                    }
                };
                match result {
                    Ok(crate::server::http::Exchange::Stream(handler)) => {
                        // A streamed response owns this connection until it
                        // closes. The fallback writes the shared 503 if the
                        // stream budget is full before any handler bytes go
                        // out.
                        drive_fallback(
                            &mut stream,
                            handler,
                            head_only,
                            &self.budget,
                            &config.limits,
                        );
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
            // Every driver asks the same ordered claim hook, so endpoint
            // precedence is not re-spelled here.
            if let Some(claim) = self.routes.claim(&head, peer, &lan, self.raw_hub.clone()) {
                serve_claimed(
                    &mut stream,
                    claim,
                    &initial,
                    head_only,
                    &self.budget,
                    &config.limits,
                );
                return;
            }
            let body_policy = self.routes.body_policy(&head);
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
            if let Some(prepared) = self.routes.mcp(&request, &lan) {
                match prepared {
                    Ok(call) => {
                        let _ = crate::server::mcp::fallback::serve_mcp_sse(
                            &mut stream,
                            call,
                            config.paths.control_dir(),
                            head_only,
                            &self.budget,
                            &config.limits,
                        );
                        return;
                    }
                    Err(response) => {
                        let _ = write_http(&mut stream, &response, head_only);
                        return;
                    }
                }
            }
            let mut response = (self.fallback)(&request, &lan);
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
}

/// Write what a pre-body face claimed.
///
/// `transfer` selects the upload path: the transfer timeout replaces the idle
/// timeout and the already-read bytes seed the handler. The UI stream keeps the
/// connection's idle timeout and needs neither.
fn serve_claimed(
    stream: &mut TcpStream,
    claim: PreBodyClaim,
    initial: &[u8],
    head_only: bool,
    budget: &crate::server::http::budget::StreamBudget,
    limits: &crate::platform::limits::Limits,
) {
    let transfer = claim.transfer;
    let handler = match claim.exchange {
        Err(response) => {
            let _ = write_http(stream, &response, head_only);
            return;
        }
        Ok(Exchange::Response(response)) if !transfer => {
            let _ = write_http(stream, &response.into_bytes(), head_only);
            return;
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
            return;
        }
        Ok(Exchange::Stream(handler)) => handler,
    };
    if transfer {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(limits.transfer_timeout));
        let _ = stream.set_write_timeout(Some(limits.transfer_timeout));
        if crate::server::http::fallback::drive(stream, handler, initial, budget, limits).is_err() {
            let _ = write_http(
                stream,
                &crate::server::http::stream_busy_response(),
                head_only,
            );
        }
    } else {
        drive_fallback(stream, handler, head_only, budget, limits);
    }
}

/// Drive a face-produced stream on the Windows connection thread. The
/// fallback owns the socket and the shared max_streams slot; a Busy result is
/// rendered before any handler bytes are written.
pub(crate) fn drive_fallback(
    stream: &mut TcpStream,
    handler: Box<dyn crate::server::http::StreamHandler>,
    head_only: bool,
    budget: &crate::server::http::budget::StreamBudget,
    limits: &crate::platform::limits::Limits,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(limits.transfer_timeout));
    if crate::server::http::fallback::drive(stream, handler, &[], budget, limits).is_err() {
        let _ = write_http(
            stream,
            &crate::server::http::stream_busy_response(),
            head_only,
        );
    }
}

/// Refuse one accepted connection with the same busy response the Unix reactor
/// sends, instead of starting a thread that would exceed the connection cap.
fn reject_connection(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = stream.write_all(engine::BUSY_CONNECTION_RESPONSE);
}

/// Why no complete request head arrived. A timeout or EOF is a closed
/// connection, not a malformed request: answering it with a 400 would be a
/// response the client never asked for and the Unix reactor does not send one.
pub(crate) enum HeadFailure {
    Closed,
    Response(Vec<u8>),
}

pub(crate) fn read_head(
    stream: &mut TcpStream,
    max_header: usize,
) -> Result<(http::Head, Vec<u8>), HeadFailure> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if buffer.len() > max_header {
            return Err(HeadFailure::Response(http::json_error(
                400,
                "bad_request",
                "header is too large",
            )));
        }
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(_) => return Err(HeadFailure::Closed),
        };
        if read == 0 {
            return Err(HeadFailure::Closed);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = http::header_end(&buffer) {
            if position > max_header {
                return Err(HeadFailure::Response(http::json_error(
                    400,
                    "bad_request",
                    "header is too large",
                )));
            }
            let Some((head, body_start)) = http::parse_head(&buffer[..position + 4]) else {
                return Err(HeadFailure::Response(http::json_error(
                    400,
                    "bad_request",
                    "malformed request line",
                )));
            };
            return Ok((head, buffer[body_start..].to_vec()));
        }
    }
}

pub(crate) fn read_body(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    body: crate::server::http::BodyPolicy,
) -> Result<Vec<u8>, Vec<u8>> {
    let framing = http::framing(&head.headers)
        .map_err(|message| http::json_error(400, "bad_request", &message))?;
    let limit = body.limit();
    if !body.accepts(framing) {
        return Err(http::json_error(
            413,
            "payload_too_large",
            "request body is too large",
        ));
    }
    if body.confirms_body(head, framing) {
        stream
            .write_all(http::CONTINUE_RESPONSE)
            .map_err(|_| http::json_error(400, "bad_request", "cannot write response"))?;
    }
    http::collect_body(stream, framing, limit, initial).map_err(|error| {
        http::json_error(
            error.status(),
            http::code_for(error.status()),
            error.detail(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The thread driver ties one admitted connection to one thread, so the
    /// admission lease is the thread cap. Prove both caps reject without
    /// consuming a slot and that dropping a lease frees the global and per-IP
    /// counts.
    #[test]
    fn admission_enforces_global_and_per_ip_caps_and_releases() {
        let admission = admission(2, 1);

        let first = admit(&admission, "10.0.0.1");
        assert!(first.is_some(), "first connection is admitted");
        assert!(
            admit(&admission, "10.0.0.1").is_none(),
            "the second connection from one IP hits max_connections_per_ip"
        );
        let second = admit(&admission, "10.0.0.2");
        assert!(second.is_some(), "a different IP still fits the global cap");
        assert!(
            admit(&admission, "10.0.0.3").is_none(),
            "a third connection hits max_connections"
        );
        assert_eq!(active(&admission), 2, "rejections do not consume slots");

        drop(first);
        assert_eq!(active(&admission), 1, "the lease drop frees one slot");
        assert!(
            admit(&admission, "10.0.0.1").is_some(),
            "the per-IP slot is reusable after the lease is dropped"
        );
    }

    fn admit(admission: &Arc<Mutex<Admission>>, peer: &str) -> Option<ConnectionLease> {
        ConnectionLease::acquire(admission, peer.parse().expect("peer ip"))
    }

    fn active(admission: &Arc<Mutex<Admission>>) -> usize {
        Admission::lock(admission).active
    }
}
