//! Temporary legacy bridge for the server loop.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module keeps the endpoint state machine and the old
//! worker/takeover path alive until #115-#125 move them to the face. The queue
//! is bounded; a full queue answers 503 instead of growing without limit.

use std::os::unix::io::{AsRawFd, RawFd};

use crate::server::http::driver::{StreamAdmission, StreamDriver};
use crate::server::http::engine::{self, HttpRequest, LegacyBridge, PreRequest, Table};
use crate::server::http::Exchange;

// Temporary bridge: this module is the only consumer of these legacy `api`
// helpers and disappears with them in #125. Keeping `super::*` here makes the
// ownership move mechanical; add explicit imports only if this module survives.
use super::*;

/// Maximum legacy jobs waiting for the four HTTP workers. A full queue is a
/// visible 503, not an unbounded allocation.
pub(super) const LEGACY_QUEUE_LIMIT: usize = 64;

pub(crate) fn start(
    listener: TcpListener,
    lan: Arc<Mutex<Lan>>,
    config: PathBuf,
    raw_hub: Arc<crate::rawhub::RawHub>,
) -> std::io::Result<()> {
    thread::Builder::new()
        .name("svcd-server-loop".into())
        .stack_size(threads::transfer_stack())
        .spawn(move || {
            let client = ClientReactor::start().ok();
            let (results_tx, results_rx) = mpsc::channel();
            let (notify_rx, notify_tx) = match UnixStream::pair() {
                Ok((rx, tx)) => (rx, tx),
                Err(error) => {
                    log::svcd(&format!("svcd: server-loop notifier failed: {}", error));
                    return;
                }
            };
            let _ = notify_rx.set_nonblocking(true);
            let _ = notify_tx.set_nonblocking(true);

            let queue = Arc::new(JobQueue {
                queue: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
                limit: LEGACY_QUEUE_LIMIT,
            });
            for _ in 0..4 {
                let queue = queue.clone();
                let config = config.clone();
                let lan = lan.clone();
                let results = results_tx.clone();
                let notify = notify_tx.try_clone().ok();
                let _ = thread::Builder::new()
                    .name("svcd-http-worker".into())
                    .stack_size(threads::request_stack())
                    .spawn(move || http_worker(queue, config, lan, results, notify));
            }

            let mut handler = ServerHandler {
                lan,
                config,
                raw_hub,
                client,
                queue,
                results: results_rx,
                notify: notify_rx,
            };
            engine::run(listener, &mut handler, crate::limits::current());
        })?;
    Ok(())
}

struct ServerHandler {
    lan: Arc<Mutex<Lan>>,
    config: PathBuf,
    raw_hub: Arc<crate::rawhub::RawHub>,
    client: Option<Arc<ClientReactor>>,
    queue: Arc<JobQueue>,
    results: Receiver<WorkResult>,
    notify: UnixStream,
}

impl LegacyBridge for ServerHandler {
    type State = ConnState;

    fn new_state(&mut self, _id: u64, _peer: IpAddr) -> ConnState {
        ConnState::Reading
    }

    fn on_accept(&mut self, _connection: &mut Connection) {}

    fn body_limit(&self, path: &str) -> u64 {
        super::body_limit(path)
    }

    fn reads_request(&self, connection: &Connection) -> bool {
        matches!(connection.state, ConnState::Reading)
    }

    fn pre_request(&mut self, connection: &mut Connection) -> PreRequest {
        if let Some(outcome) = self.plugin_ui_stream(connection) {
            return outcome;
        }
        if let Some((stream, head, route)) = export_takeover(connection) {
            let lan = self.lan.clone();
            let config = self.config.clone();
            let _ = thread::Builder::new()
                .name("svcd-export-http".into())
                .stack_size(threads::transfer_stack())
                .spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_nonblocking(false);
                    crate::export_http::serve(&mut stream, &head, &lan, &config, route);
                });
            return PreRequest::Close;
        }
        if let Some((stream, head, initial)) = import_takeover(connection) {
            let lan = self.lan.clone();
            let config = self.config.clone();
            let limits = crate::limits::current();
            let _ = thread::Builder::new()
                .name("svcd-import-http".into())
                .stack_size(threads::transfer_stack())
                .spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_nonblocking(false);
                    crate::install_http::serve_import(
                        &mut stream,
                        &head,
                        &initial,
                        &lan,
                        &config,
                        limits,
                    );
                });
            return PreRequest::Close;
        }
        if let Some((stream, head, initial)) = install_takeover(connection) {
            let lan = self.lan.clone();
            let config = self.config.clone();
            let limits = crate::limits::current();
            let _ = thread::Builder::new()
                .name("svcd-install-http".into())
                .stack_size(threads::transfer_stack())
                .spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_nonblocking(false);
                    crate::install_http::serve(&mut stream, &head, &initial, &lan, &config, limits);
                });
            return PreRequest::Close;
        }
        if let Some((stream, head, initial, route)) = blob_takeover(connection) {
            let lan = self.lan.clone();
            let _ = thread::Builder::new()
                .name("svcd-blob-http".into())
                .stack_size(threads::transfer_stack())
                .spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_nonblocking(false);
                    let _ = blob_http::serve(&mut stream, &head, &initial, &lan, route);
                });
            return PreRequest::Close;
        }
        PreRequest::Continue
    }

    fn on_request(&mut self, connection: &mut Connection, request: HttpRequest) {
        let keep_alive = wants_keep_alive(&request);
        if let Some(response) = serve_face(&request, connection.peer, &self.lan, keep_alive) {
            write_response(connection, response, keep_alive);
            return;
        }
        let path = request
            .path
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(request.path.as_str());
        if path == "/mcp" {
            let password_hash = self
                .lan
                .lock()
                .ok()
                .and_then(|state| state.password_hash().map(str::to_string));
            let prepared = mcp_http::prepare_streaming(
                &mcp_http::Request {
                    method: &request.method,
                    headers: &request.headers,
                    body: &request.body,
                },
                password_hash.as_deref(),
            );
            match prepared {
                Ok(Some(call)) => {
                    if let Some(client) = &self.client {
                        start_mcp_sse(connection, call, self.config.clone(), client);
                    } else {
                        write_response(
                            connection,
                            http::json_error(
                                500,
                                "stream_unavailable",
                                "management client reactor is unavailable",
                            ),
                            false,
                        );
                    }
                    return;
                }
                Ok(None) => {}
                Err(response) => {
                    write_response(connection, response, false);
                    return;
                }
            }
        }
        if let Some(kind) = ui_stream_kind(&request.path) {
            if kind == "events" {
                match prepare_sse(&request, &self.lan) {
                    Ok((receiver, subscription, replay)) => {
                        start_sse(connection, receiver, subscription, replay);
                    }
                    Err(response) => {
                        write_response(connection, response, false);
                    }
                }
                return;
            }
            if kind == "raw" {
                match prepare_raw_ws(&request, &self.lan, &self.raw_hub) {
                    Ok((_plugin, accept, subscription)) => {
                        start_raw_ws(connection, accept, subscription);
                    }
                    Err(response) => {
                        write_response(connection, response, false);
                    }
                }
                return;
            }
            match prepare_ws(&request, &self.lan) {
                Ok((plugin, accept, receiver, subscription)) => {
                    start_ws(connection, plugin, accept, receiver, subscription);
                }
                Err(response) => {
                    write_response(connection, response, false);
                }
            }
            return;
        }
        let job = Work::Http(HttpJob {
            connection: connection.id,
            request,
            keep_alive,
        });
        if self.queue.try_push(job).is_err() {
            write_response(connection, bridge_busy(), false);
        } else {
            connection.state = ConnState::Processing;
        }
    }

    fn on_error(&mut self, connection: &mut Connection, response: Vec<u8>) {
        write_response(connection, response, false);
    }

    fn on_readable(&mut self, connection: &mut Connection) -> bool {
        if matches!(&connection.state, ConnState::Stream(_)) {
            return stream_readable(connection);
        }
        match &connection.state {
            ConnState::McpSse(_) => {
                let mut discard = [0u8; 512];
                match connection.stream.read(&mut discard) {
                    Ok(0) => return false,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return false,
                }
                fill_mcp_sse(connection);
                true
            }
            ConnState::Sse(_) => {
                let mut discard = [0u8; 512];
                match connection.stream.read(&mut discard) {
                    Ok(0) => return false,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => return false,
                }
                fill_sse(connection);
                true
            }
            ConnState::Ws(_) => drive_ws(connection, connection.id, &self.queue, true),
            ConnState::RawWs(_) => {
                drive_raw_ws(connection, connection.id, &self.queue, true, &self.raw_hub)
            }
            _ => true,
        }
    }

    fn on_writable(&mut self, connection: &mut Connection) -> bool {
        if matches!(&connection.state, ConnState::Stream(_)) {
            return stream_writable(connection);
        }
        match &connection.state {
            ConnState::McpSse(_) => {
                fill_mcp_sse(connection);
                if !connection.write_out() {
                    return false;
                }
                let done = matches!(&connection.state, ConnState::McpSse(state) if state.done);
                !(done && connection.out.is_empty())
            }
            ConnState::Ws(_) => drive_ws(connection, connection.id, &self.queue, false),
            ConnState::RawWs(_) => {
                drive_raw_ws(connection, connection.id, &self.queue, false, &self.raw_hub)
            }
            ConnState::Sse(_) => {
                fill_sse(connection);
                connection.write_out()
            }
            _ => {
                if !connection.write_out() {
                    return false;
                }
                if connection.out_at >= connection.out.len() {
                    if connection.keep_alive {
                        connection.state = ConnState::Reading;
                        true
                    } else {
                        false
                    }
                } else {
                    true
                }
            }
        }
    }

    fn on_tick(&mut self, connections: &mut Table<ConnState>) {
        let mut drain = [0u8; 64];
        while self.notify.read(&mut drain).is_ok() {}

        while let Ok(result) = self.results.try_recv() {
            match result {
                WorkResult::Http(result) => {
                    if let Some(connection) = connections.get_mut(&result.connection) {
                        write_response(connection, result.response, result.keep_alive);
                    }
                }
                WorkResult::Ws(result) => {
                    if let Some(connection) = connections.get_mut(&result.connection) {
                        if matches!(&connection.state, ConnState::Ws(_)) {
                            push_ws_text(&mut connection.out, &result.text);
                            if let ConnState::Ws(state) = &mut connection.state {
                                state.pending = false;
                                if let Some(payload) = state.queued.pop_front() {
                                    state.pending = true;
                                    state.queued_bytes =
                                        state.queued_bytes.saturating_sub(payload.len());
                                    self.queue.push(Work::Ws(WsJob {
                                        connection: result.connection,
                                        plugin: state.plugin.clone(),
                                        payload,
                                    }));
                                }
                            }
                        }
                    }
                }
            }
        }

        for connection in connections.values_mut() {
            let ConnState::RawWs(state) = &mut connection.state else {
                continue;
            };
            let mut closed = None;
            loop {
                match state.subscription.try_recv() {
                    Ok(crate::rawhub::RawEvent::Frame(payload)) => {
                        push_ws_frame(&mut connection.out, 0x2, &payload);
                    }
                    Ok(crate::rawhub::RawEvent::Closed) => {
                        closed = Some("raw closed");
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        closed = Some("raw channel closed");
                        break;
                    }
                }
            }
            if let Some(reason) = closed {
                raw_ws_log(state, reason);
                push_ws_frame(&mut connection.out, 0x8, &close_payload(1000, "raw closed"));
                connection.keep_alive = false;
                connection.state = ConnState::Writing;
                continue;
            }
            if state.heartbeat.elapsed() >= Duration::from_secs(30) {
                push_ws_frame(&mut connection.out, 0x9, b"");
                state.heartbeat = Instant::now();
            }
        }

        let queue = self.queue.clone();
        let raw_hub = self.raw_hub.clone();
        let ids: Vec<u64> = connections.keys().copied().collect();
        for id in ids {
            let keep = if let Some(connection) = connections.get_mut(&id) {
                match &connection.state {
                    ConnState::Ws(_) => drive_ws(connection, id, &queue, false),
                    ConnState::RawWs(_) => drive_raw_ws(connection, id, &queue, false, &raw_hub),
                    _ => true,
                }
            } else {
                true
            };
            if !keep {
                if let Some(connection) = connections.remove(&id) {
                    release_mcp_stream(&connection, self.client.as_deref());
                }
            }
        }

        let now = Instant::now();
        let stream_ids: Vec<u64> = connections
            .iter()
            .filter(|(_, connection)| matches!(&connection.state, ConnState::Stream(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in stream_ids {
            let keep = match connections.get_mut(&id) {
                Some(connection) => stream_tick(connection, now),
                None => true,
            };
            if !keep {
                if let Some(mut connection) = connections.remove(&id) {
                    self.on_close(&mut connection);
                }
            }
        }

        for connection in connections.values_mut() {
            match &connection.state {
                ConnState::Sse(_) => fill_sse(connection),
                ConnState::McpSse(_) => fill_mcp_sse(connection),
                _ => {}
            }
        }
    }

    fn interest(&self, connection: &Connection) -> i16 {
        let mut events = match connection.state {
            ConnState::Reading => libc::POLLIN,
            ConnState::Writing => libc::POLLOUT,
            ConnState::Sse(_) => libc::POLLIN,
            ConnState::Ws(_) => libc::POLLIN,
            ConnState::RawWs(_) => libc::POLLIN,
            ConnState::McpSse(_) => libc::POLLIN,
            ConnState::Stream(_) => libc::POLLIN,
            ConnState::Processing => 0,
        };
        if connection.out_at < connection.out.len() || stream_has_file(connection) {
            events |= libc::POLLOUT;
        }
        events
    }

    fn is_idle(&self, connection: &Connection, now: Instant, idle: Duration) -> bool {
        matches!(connection.state, ConnState::Reading | ConnState::Writing)
            && now.saturating_duration_since(connection.last_active) > idle
    }

    fn on_close(&mut self, connection: &mut Connection) {
        // Replacing or dropping ConnState::Stream drops the driver, which
        // delivers the handler's `Closed` event.
        release_mcp_stream(connection, self.client.as_deref());
    }

    fn on_poll_error(&mut self, connection: &mut Connection, revents: i16) {
        if let ConnState::RawWs(state) = &connection.state {
            raw_ws_log(state, &format!("poll flags {:#x}", revents));
        }
    }

    fn wake_fd(&self) -> Option<RawFd> {
        Some(self.notify.as_raw_fd())
    }
}

impl ServerHandler {
    /// The plugin UI asset path: decide the route before the body is read,
    /// resolve the principal once, then attach the sans-IO handler to the
    /// connection. Returns \`None\` when this request is not a static asset.
    fn plugin_ui_stream(&self, connection: &mut Connection) -> Option<PreRequest> {
        if connection.pending.is_some() {
            return None;
        }
        let header_end = http::header_end(&connection.buffer)?;
        if header_end > crate::limits::current().max_header {
            return None;
        }
        let (head, body_start) = http::parse_head(&connection.buffer[..header_end + 4])?;
        // Claim the path before authentication so an unauthenticated browser
        // navigation gets the login redirect; the face applies the endpoint
        // policy after the server resolved the principal.
        crate::face::plugin_ui::PluginUiFace::route_for(&head.method, &head.path)?;
        let exchange =
            match crate::face::plugin_ui::PluginUiFace::exchange(&head, connection.peer, &self.lan)
            {
                Ok(exchange) => exchange,
                Err(response) => {
                    write_response(connection, response, false);
                    return Some(PreRequest::Handled);
                }
            };
        let Exchange::Stream(handler) = exchange else {
            write_response(
                connection,
                http::json_error(
                    500,
                    "internal",
                    "a streamed route returned a memory response",
                ),
                false,
            );
            return Some(PreRequest::Handled);
        };

        // Take the head (and any already-read body bytes) out of the request
        // buffer; the loop must not try to parse them again.
        let initial = connection.buffer.split_off(body_start);
        connection.buffer.clear();
        connection.keep_alive = false;

        let mut driver = match StreamDriver::admit(handler, crate::limits::current().max_streams) {
            StreamAdmission::Admitted(driver) => driver,
            StreamAdmission::Busy => {
                write_response(connection, stream_busy(), false);
                return Some(PreRequest::Handled);
            }
        };
        let primed = driver.prime(
            &mut connection.stream,
            &mut connection.out,
            &mut connection.out_at,
            &mut connection.last_active,
        );
        connection.state = ConnState::Stream(driver);
        if !primed {
            finish_stream(connection);
            return Some(PreRequest::Handled);
        }
        if !initial.is_empty() {
            let keep = {
                let Connection {
                    state, out, out_at, ..
                } = connection;
                match state {
                    ConnState::Stream(driver) => driver.on_data(&initial, out, out_at),
                    _ => true,
                }
            };
            if !keep {
                finish_stream(connection);
            }
        }
        Some(PreRequest::Handled)
    }
}

/// End a stream whose output buffer is already empty. Replacing the state
/// drops the driver, which delivers the handler's \`Closed\` event.
fn finish_stream(connection: &mut Connection) {
    connection.out.clear();
    connection.out_at = 0;
    connection.keep_alive = false;
    connection.state = ConnState::Writing;
}

fn stream_has_file(connection: &Connection) -> bool {
    matches!(&connection.state, ConnState::Stream(driver) if driver.has_file())
}

fn stream_readable(connection: &mut Connection) -> bool {
    let Connection {
        state,
        stream,
        out,
        out_at,
        last_active,
        ..
    } = connection;
    let ConnState::Stream(driver) = state else {
        return true;
    };
    driver.on_readable(stream, out, out_at, last_active)
}

fn stream_writable(connection: &mut Connection) -> bool {
    let Connection {
        state,
        stream,
        out,
        out_at,
        last_active,
        ..
    } = connection;
    let ConnState::Stream(driver) = state else {
        return true;
    };
    driver.on_writable(stream, out, out_at, last_active)
}

fn stream_tick(connection: &mut Connection, now: Instant) -> bool {
    let Connection {
        state,
        stream,
        out,
        out_at,
        last_active,
        ..
    } = connection;
    let ConnState::Stream(driver) = state else {
        return true;
    };
    driver.on_tick(now, stream, out, out_at, last_active)
}

fn stream_busy() -> Vec<u8> {
    let body = json!({ "error": { "code": "busy", "message": "stream cap reached" } });
    http::response_with_headers(
        503,
        "application/json",
        &[("Retry-After".to_string(), "1".to_string())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn write_response(connection: &mut Connection, response: Vec<u8>, keep_alive: bool) {
    connection.out = response;
    connection.out_at = 0;
    connection.keep_alive = keep_alive;
    connection.state = ConnState::Writing;
}

fn bridge_busy() -> Vec<u8> {
    const BODY: &[u8] = br#"{"error":{"code":"busy","message":"legacy request queue is full"}}"#;
    http::response_with_headers(
        503,
        "application/json",
        &[("Retry-After".to_string(), "1".to_string())],
        BODY,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_bridge_answers_503_with_retry_after() {
        let response = bridge_busy();
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{text}"
        );
        assert!(text.contains("Retry-After: 1"), "{text}");
        assert!(text.contains("legacy request queue is full"), "{text}");
    }
}
