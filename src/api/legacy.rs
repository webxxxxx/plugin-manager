//! Temporary legacy bridge for the server loop.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module keeps the endpoint state machine and the old
//! worker/takeover path alive until #115-#125 move them to the face. The queue
//! is bounded; a full queue answers 503 instead of growing without limit.

use std::os::unix::io::{AsRawFd, RawFd};

use crate::server::http::engine::{self, HttpRequest, LegacyBridge, PreRequest, Table};
use crate::server::http::{
    dispatch_inline, CloseReason, Exchange, OutBuffer, Request, StreamAction, StreamEvent,
    StreamHandler, StreamStep,
};

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
            return drive_stream_writable(connection);
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
                Some(connection) => drive_stream_tick(connection, now),
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
        if let ConnState::Stream(state) = &mut connection.state {
            let reason = state.closing.clone().unwrap_or(CloseReason::PeerClosed);
            let mut discarded = Vec::new();
            let _ = state
                .handler
                .on_event(StreamEvent::Closed(reason), &mut OutBuffer::new(&mut discarded, 0));
        }
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
        let route = crate::face::plugin_ui::PluginUiFace::route_for(&head.method, &head.path)?;
        let path = head
            .path
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(head.path.as_str());

        // The server owns transport authentication. A failure is presented by
        // the server (login redirect for a browser, 401 for an API client);
        // the face never runs without a principal.
        let principal = match self.lan.lock() {
            Ok(mut state) => state.authenticate(&head.headers),
            Err(_) => Err(()),
        };
        let principal = match principal {
            Ok(principal) => principal,
            Err(_) => {
                let response = crate::face::plugin_ui::unauthenticated(&head.headers, path);
                write_response(connection, response, false);
                return Some(PreRequest::Handled);
            }
        };

        let request = Request {
            method: head.method.clone(),
            path: head.path.clone(),
            headers: head.headers.clone(),
            peer: connection.peer,
            principal,
            body: Vec::new(),
        };
        let face = crate::face::plugin_ui::PluginUiFace::new(self.lan.clone());
        let exchange = match dispatch_inline(&face, &route, request) {
            Ok(exchange) => exchange,
            Err(error) => {
                write_response(
                    connection,
                    http::json_error(500, "internal", &error.message()),
                    false,
                );
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
        stream_attach(connection, handler);
        if matches!(connection.state, ConnState::Stream(_)) && !initial.is_empty() {
            if !drive_stream_event(connection, StreamEvent::Data(&initial)) {
                finish_stream(connection);
            }
        }
        Some(PreRequest::Handled)
    }
}

/// Attach a handler and emit its first response bytes immediately. The engine
/// only calls \`on_writable\` after a poll event, so the head would otherwise wait
/// for the next loop tick.
fn stream_attach(connection: &mut Connection, handler: Box<dyn StreamHandler>) {
    let class = handler.stream_class();
    connection.state = ConnState::Stream(StreamState {
        handler,
        class,
        deadline: Instant::now() + crate::limits::current().transfer_timeout,
        file: None,
        closing: None,
    });
    if !drive_stream_writable(connection) {
        finish_stream(connection);
    }
}

fn stream_has_file(connection: &Connection) -> bool {
    matches!(&connection.state, ConnState::Stream(state) if state.file.is_some())
}

fn stream_closing(connection: &Connection) -> bool {
    matches!(&connection.state, ConnState::Stream(state) if state.closing.is_some())
}

/// End a stream whose output buffer is already empty. The handler gets its
/// \`Closed\` event before it is dropped.
fn finish_stream(connection: &mut Connection) {
    if let ConnState::Stream(state) = &mut connection.state {
        let reason = state.closing.clone().unwrap_or(CloseReason::Completed);
        let mut discarded = Vec::new();
        let _ = state.handler.on_event(
            StreamEvent::Closed(reason),
            &mut OutBuffer::new(&mut discarded, 0),
        );
    }
    connection.out.clear();
    connection.out_at = 0;
    connection.keep_alive = false;
    connection.state = ConnState::Writing;
}

/// Feed one event to the attached handler and apply its action.
fn drive_stream_event(connection: &mut Connection, event: StreamEvent<'_>) -> bool {
    let limit = crate::limits::current().stream_buffer;
    let before = connection.out.len();
    let step = {
        let ConnState::Stream(state) = &mut connection.state else {
            return true;
        };
        state
            .handler
            .on_event(event, &mut OutBuffer::new(&mut connection.out, limit))
    };
    let wrote = connection.out.len() > before;
    apply_stream_step(connection, step, wrote)
}

fn apply_stream_step(connection: &mut Connection, step: StreamStep, wrote: bool) -> bool {
    let now = Instant::now();
    let transfer = crate::limits::current().transfer_timeout;
    let ConnState::Stream(state) = &mut connection.state else {
        return false;
    };
    if wrote {
        state.deadline = now + transfer;
    }
    if let Some(requested) = step.deadline {
        if requested < state.deadline {
            state.deadline = requested;
        }
    }
    match step.action {
        StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {}
        // #116 owns the pool-aware path for faces that submit work. A stream
        // handler in this slice produces its bytes inline.
        StreamAction::SubmitBlocking(_) => {
            state.closing = Some(CloseReason::Failed(
                "stream handlers cannot submit blocking jobs in this slice".into(),
            ));
        }
        StreamAction::SendFile(send) => {
            state.file = Some(PendingFile {
                file: send.file,
                offset: send.offset,
                length: send.length,
            });
            // A file send is the last thing this response does.
            state.closing = Some(CloseReason::Completed);
        }
        StreamAction::Close(reason) => {
            state.closing = Some(reason);
        }
    }
    if state.closing.is_some() && state.file.is_none() && connection.out_at >= connection.out.len()
    {
        return false;
    }
    true
}

/// Flush, then either send a queued file or let the handler write the next
/// chunk. Returns false when the connection should be removed.
fn drive_stream_writable(connection: &mut Connection) -> bool {
    loop {
        let before = connection.out_at;
        if !connection.write_out() {
            return false;
        }
        if connection.out_at > before {
            if let ConnState::Stream(state) = &mut connection.state {
                state.deadline = Instant::now() + crate::limits::current().transfer_timeout;
            }
        }
        if connection.out_at < connection.out.len() {
            return true;
        }
        if !connection.out.is_empty() {
            connection.out.clear();
        }
        connection.out_at = 0;

        if stream_has_file(connection) {
            if !pump_stream_file(connection) {
                return false;
            }
            if connection.out_at < connection.out.len() || stream_has_file(connection) {
                return true;
            }
            continue;
        }
        if stream_closing(connection) {
            return false;
        }
        if !drive_stream_event(connection, StreamEvent::Writable) {
            return false;
        }
        if connection.out_at < connection.out.len() || stream_has_file(connection) {
            return true;
        }
        // The handler accepted the event without producing bytes; wait for the
        // next poll instead of spinning.
        return true;
    }
}

/// Advance the server-side file send. One-shot assets keep the zero-copy
/// kernel path where the platform has an incremental one; elsewhere the
/// bounded read+write fallback keeps memory inside the stream buffer.
fn pump_stream_file(connection: &mut Connection) -> bool {
    let limit = crate::limits::current().stream_buffer;
    loop {
        let ConnState::Stream(state) = &mut connection.state else {
            return false;
        };
        let Some(mut pending) = state.file.take() else {
            return true;
        };
        if pending.length == 0 {
            continue;
        }
        match crate::zerocopy::try_send_file_range(
            &mut pending.file,
            &mut connection.stream,
            pending.offset,
            pending.length,
        ) {
            Ok(crate::zerocopy::FileSendProgress::Sent(sent)) => {
                pending.offset += sent;
                pending.length -= sent;
                state.file = Some(pending);
                state.deadline = Instant::now() + crate::limits::current().transfer_timeout;
                continue;
            }
            Ok(crate::zerocopy::FileSendProgress::Done) => {
                state.deadline = Instant::now() + crate::limits::current().transfer_timeout;
                continue;
            }
            Ok(crate::zerocopy::FileSendProgress::WouldBlock) => {
                state.file = Some(pending);
                return true;
            }
            Ok(crate::zerocopy::FileSendProgress::Unsupported) => {
                match read_file_chunk(&mut pending, &mut connection.out, limit) {
                    Ok(0) => {
                        // The output buffer is full; flush it before reading.
                        state.file = Some(pending);
                        return true;
                    }
                    Ok(_) => {
                        state.file = Some(pending);
                        state.deadline = Instant::now() + crate::limits::current().transfer_timeout;
                        return true;
                    }
                    Err(error) => {
                        log::svcd(&format!("svcd: plugin UI asset read failed: {}", error));
                        state.closing =
                            Some(CloseReason::Failed(format!("read UI asset: {}", error)));
                        return true;
                    }
                }
            }
            Err(error) => {
                log::svcd(&format!("svcd: plugin UI file send failed: {}", error));
                state.closing = Some(CloseReason::Failed(format!("send UI asset: {}", error)));
                return true;
            }
        }
    }
}

/// Read the next bounded chunk of an asset into the connection's output
/// buffer. Returns 0 when the buffer is full rather than at EOF.
fn read_file_chunk(
    pending: &mut PendingFile,
    out: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize> {
    use std::io::{Read, Seek, SeekFrom};

    let capacity = limit.saturating_sub(out.len());
    let want = std::cmp::min(capacity as u64, pending.length) as usize;
    if want == 0 {
        return Ok(0);
    }
    pending.file.seek(SeekFrom::Start(pending.offset))?;
    let base = out.len();
    out.resize(base + want, 0);
    match pending.file.read(&mut out[base..]) {
        Ok(0) => {
            out.truncate(base);
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "UI asset ended before its advertised length",
            ))
        }
        Ok(read) => {
            out.truncate(base + read);
            pending.offset += read as u64;
            pending.length -= read as u64;
            Ok(read)
        }
        Err(error) => {
            out.truncate(base);
            Err(error)
        }
    }
}

/// Deliver a stream's deadline event. The server keeps the upper bound: a
/// handler that neither made progress nor asked for a later wakeup closes.
fn drive_stream_tick(connection: &mut Connection, now: Instant) -> bool {
    let due = match &connection.state {
        ConnState::Stream(state) => now >= state.deadline,
        _ => false,
    };
    if !due {
        return true;
    }
    if !drive_stream_event(connection, StreamEvent::Tick(now)) {
        return false;
    }
    let still_due = match &connection.state {
        ConnState::Stream(state) => now >= state.deadline,
        _ => false,
    };
    if still_due {
        if let ConnState::Stream(state) = &mut connection.state {
            state.closing = Some(CloseReason::TimedOut);
        }
        return false;
    }
    true
}

/// Read request bytes for a streamed route and hand them to the handler.
fn stream_readable(connection: &mut Connection) -> bool {
    let mut chunk = [0u8; 8 * 1024];
    match connection.stream.read(&mut chunk) {
        Ok(0) => false,
        Ok(read) => {
            if let ConnState::Stream(state) = &mut connection.state {
                state.deadline = Instant::now() + crate::limits::current().transfer_timeout;
            }
            drive_stream_event(connection, StreamEvent::Data(&chunk[..read]))
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
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
