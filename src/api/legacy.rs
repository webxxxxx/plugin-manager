//! Temporary legacy bridge for the server loop.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module keeps the endpoint state machine and the old
//! worker/takeover path alive until #115-#125 move them to the face. The queue
//! is bounded; a full queue answers 503 instead of growing without limit.

use std::os::unix::io::{AsRawFd, RawFd};

use crate::server::http::engine::{self, Handler, HttpRequest, PreRequest, Table};

use super::*;

/// Maximum legacy jobs waiting for the four HTTP workers. A full queue is a
/// visible 503, not an unbounded allocation.
const LEGACY_QUEUE_LIMIT: usize = 64;

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

impl Handler for ServerHandler {
    type State = ConnState;

    fn new_state(&mut self, _id: u64, _peer: IpAddr) -> ConnState {
        ConnState::Reading
    }

    fn on_accept(&mut self, _connection: &mut Connection) {}

    fn reads_request(&self, connection: &Connection) -> bool {
        matches!(connection.state, ConnState::Reading)
    }

    fn pre_request(&mut self, connection: &mut Connection) -> PreRequest {
        if let Some((stream, head)) = ui_takeover(connection) {
            let lan = self.lan.clone();
            let _ = thread::Builder::new()
                .name("svcd-ui-http".into())
                .stack_size(threads::transfer_stack())
                .spawn(move || {
                    let mut stream = stream;
                    let _ = stream.set_nonblocking(false);
                    crate::ui_http::serve(&mut stream, &head, &lan);
                });
            return PreRequest::Close;
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
            connection.out = response;
            connection.out_at = 0;
            connection.keep_alive = keep_alive;
            connection.state = ConnState::Writing;
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
                        connection.out = http::json_error(
                            500,
                            "stream_unavailable",
                            "management client reactor is unavailable",
                        );
                        connection.out_at = 0;
                        connection.keep_alive = false;
                        connection.state = ConnState::Writing;
                    }
                    return;
                }
                Ok(None) => {}
                Err(response) => {
                    connection.out = response;
                    connection.out_at = 0;
                    connection.keep_alive = false;
                    connection.state = ConnState::Writing;
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
                        connection.out = response;
                        connection.out_at = 0;
                        connection.keep_alive = false;
                        connection.state = ConnState::Writing;
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
                        connection.out = response;
                        connection.out_at = 0;
                        connection.keep_alive = false;
                        connection.state = ConnState::Writing;
                    }
                }
                return;
            }
            match prepare_ws(&request, &self.lan) {
                Ok((plugin, accept, receiver, subscription)) => {
                    start_ws(connection, plugin, accept, receiver, subscription);
                }
                Err(response) => {
                    connection.out = response;
                    connection.out_at = 0;
                    connection.keep_alive = false;
                    connection.state = ConnState::Writing;
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
            connection.out = bridge_busy();
            connection.out_at = 0;
            connection.keep_alive = false;
            connection.state = ConnState::Writing;
        } else {
            connection.state = ConnState::Processing;
        }
    }

    fn on_error(&mut self, connection: &mut Connection, response: Vec<u8>) {
        connection.out = response;
        connection.out_at = 0;
        connection.keep_alive = false;
        connection.state = ConnState::Writing;
    }

    fn on_readable(&mut self, connection: &mut Connection) -> bool {
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
                        connection.out = result.response;
                        connection.out_at = 0;
                        connection.keep_alive = result.keep_alive;
                        connection.state = ConnState::Writing;
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
                                    let job = Work::Ws(WsJob {
                                        connection: result.connection,
                                        plugin: state.plugin.clone(),
                                        payload,
                                    });
                                    if self.queue.try_push(job).is_err() {
                                        push_ws_frame(
                                            &mut connection.out,
                                            0x8,
                                            &close_payload(1013, "busy"),
                                        );
                                        connection.keep_alive = false;
                                        connection.state = ConnState::Writing;
                                    }
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
            ConnState::Processing => 0,
        };
        if connection.out_at < connection.out.len() {
            events |= libc::POLLOUT;
        }
        events
    }

    fn is_idle(&self, connection: &Connection, now: Instant, idle: Duration) -> bool {
        matches!(connection.state, ConnState::Reading | ConnState::Writing)
            && now.saturating_duration_since(connection.last_active) > idle
    }

    fn on_close(&mut self, connection: &mut Connection) {
        release_mcp_stream(connection, self.client.as_deref());
    }

    fn wake_fd(&self) -> Option<RawFd> {
        Some(self.notify.as_raw_fd())
    }
}

fn bridge_busy() -> Vec<u8> {
    let body = serde_json::to_vec(&json!({
        "error": {
            "code": "busy",
            "message": "legacy request queue is full",
        }
    }))
    .unwrap_or_default();
    http::response_with_headers(
        503,
        "application/json",
        &[("Retry-After".to_string(), "1".to_string())],
        &body,
    )
}
