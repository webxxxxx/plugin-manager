//! Temporary legacy bridge for the server loop.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module keeps the endpoint state machine and the old
//! worker/takeover path alive until #115-#125 move them to the face. The queue
//! is bounded; a full queue answers 503 instead of growing without limit.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::{AsRawFd, RawFd};

use crate::server::http::driver::{StreamAdmission, StreamDriver};
use crate::server::http::engine::{self, HttpRequest, LegacyBridge, PreRequest, Table};
use crate::server::http::{
    complete_blocking, dispatch_inline, BlockingOutcome, BlockingPool, BodyPolicy, Busy,
    CancelToken, CompletionSlot, Dispatch, Exchange, Face, JobOutcome,
};

/// One queued or running blocking request. The server owns the completion slot
/// and the connection; the pool only reports that the job finished.
struct PendingBlocking {
    slot: CompletionSlot,
    body: BodyPolicy,
    keep_alive: bool,
}

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
    let (results_tx, results_rx) = mpsc::channel();
    let (notify_rx, notify_tx) = UnixStream::pair()?;
    let _ = notify_rx.set_nonblocking(true);
    let _ = notify_tx.set_nonblocking(true);

    let face: Arc<dyn Face> = Arc::new(crate::face::api::ApiFace::new(config.clone()));
    // The pool belongs to the server loop, not to one request or the temporary
    // bridge. Starting it before the loop means a thread-spawn failure fails
    // HTTP startup instead of silently disabling every route after start().
    let blocking = {
        let results = results_tx.clone();
        let notify = notify_tx.try_clone()?;
        BlockingPool::start(
            crate::limits::current().blocking_threads,
            crate::server::http::pool::QUEUE_LIMIT,
            move |key, outcome| {
                let _ = results.send(WorkResult::BlockingDone { key, outcome });
                let _ = (&notify).write(&[1]);
            },
        )?
    };

    thread::Builder::new()
        .name("svcd-server-loop".into())
        .stack_size(threads::transfer_stack())
        .spawn(move || {
            let client = ClientReactor::start().ok();
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
                results_tx: results_tx.clone(),
                notify: notify_rx,
                face,
                blocking,
                blocking_pending: HashMap::new(),
                stream_jobs: HashMap::new(),
                stream_job_keys: HashSet::new(),
                next_stream_job: 1 << 32,
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
    /// A sender kept for stream-handler blocking jobs; their result is
    /// delivered to the handler, not to a completion slot.
    results_tx: Sender<WorkResult>,
    notify: UnixStream,
    face: Arc<dyn Face>,
    blocking: BlockingPool,
    blocking_pending: HashMap<u64, PendingBlocking>,
    /// Connection -> the unique pool key of its in-flight stream job. Stream
    /// jobs do not use the connection id as their key: the handler can submit
    /// the next job from `StreamJobDone` before the worker clears the previous
    /// key in the pool.
    stream_jobs: HashMap<u64, u64>,
    /// Keys that belong to stream jobs, so a `BlockingDone` is not mistaken
    /// for a lost face completion.
    stream_job_keys: HashSet<u64>,
    next_stream_job: u64,
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
        if self.serve_face(connection, &request, keep_alive) {
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
            let keep = stream_readable(connection);
            if keep {
                self.service_stream_job(connection);
            }
            return keep;
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
            _ => true,
        }
    }

    fn on_writable(&mut self, connection: &mut Connection) -> bool {
        if matches!(&connection.state, ConnState::Stream(_)) {
            let keep = stream_writable(connection);
            if keep {
                self.service_stream_job(connection);
            }
            return keep;
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
                WorkResult::BlockingDone { key, outcome } => {
                    let Some(pending) = self.blocking_pending.remove(&key) else {
                        // A stream handler's job reports its value through
                        // `StreamJobDone` first; this outcome only cleans the
                        // pool slot. Cancel/drop outcomes are expected after a
                        // disconnect too. Anything else lost its bookkeeping.
                        if !self.stream_job_keys.remove(&key)
                            && outcome == JobOutcome::Finished
                            && connections.contains_key(&key)
                        {
                            log::svcd(&format!(
                                "svcd: blocking completion for connection {} lost its slot",
                                key
                            ));
                        }
                        continue;
                    };
                    let exchange = pending.slot.take();
                    let Some(connection) = connections.get_mut(&key) else {
                        // The client disconnected while the job was running.
                        // The sealed slot drops a late completion.
                        continue;
                    };
                    let (response, keep_alive) = match outcome {
                        JobOutcome::Dropped => (blocking_busy(Busy::Closed), false),
                        JobOutcome::Finished | JobOutcome::Cancelled => {
                            let mut response =
                                super::render_exchange(complete_blocking(pending.body, exchange));
                            if pending.keep_alive {
                                response = keep_alive_response(response);
                            }
                            (response, pending.keep_alive)
                        }
                    };
                    write_response(connection, response, keep_alive);
                }
                WorkResult::StreamJobDone {
                    connection: id,
                    outcome,
                } => {
                    let keep = match connections.get_mut(&id) {
                        Some(connection) => deliver_stream_outcome(connection, outcome),
                        None => false,
                    };
                    if keep {
                        if let Some(connection) = connections.get_mut(&id) {
                            self.service_stream_job(connection);
                        }
                    } else if let Some(mut connection) = connections.remove(&id) {
                        self.on_close(&mut connection);
                    }
                }
            }
        }

        // A blocking job leaves the connection in `Processing`, where the loop
        // does not request POLLIN. Check for a graceful peer FIN here so a
        // disconnect cancels the job instead of waiting for POLLHUP.
        let closed: Vec<u64> = connections
            .iter()
            .filter(|(_, connection)| {
                matches!(connection.state, ConnState::Processing) && peer_closed(connection)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in closed {
            if let Some(mut connection) = connections.remove(&id) {
                self.on_close(&mut connection);
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
                Some(connection) => {
                    let keep = stream_tick(connection, now);
                    if keep {
                        self.service_stream_job(connection);
                    }
                    keep
                }
                None => true,
            };
            if !keep {
                if let Some(mut connection) = connections.remove(&id) {
                    self.on_close(&mut connection);
                }
            }
        }

        for connection in connections.values_mut() {
            if let ConnState::McpSse(_) = &connection.state {
                fill_mcp_sse(connection);
            }
        }
    }

    fn interest(&self, connection: &Connection) -> i16 {
        let mut events = match &connection.state {
            ConnState::Reading => libc::POLLIN,
            ConnState::Writing => libc::POLLOUT,
            ConnState::McpSse(_) => libc::POLLIN,
            ConnState::Stream(driver) => {
                if driver.wants_read() {
                    libc::POLLIN
                } else {
                    0
                }
            }
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
        if let Some(pending) = self.blocking_pending.remove(&connection.id) {
            // Seal the slot first: a job that finishes after this point writes
            // into a sealed slot and is ignored. Then free a queued slot or
            // flip a running job's cancellation token.
            pending.slot.take();
            self.blocking.cancel(connection.id);
        } else if let Some(key) = self.stream_jobs.remove(&connection.id) {
            // Same contract for a stream job: the connection is gone, so a
            // queued job is dropped and a running one learns it was cancelled.
            self.stream_job_keys.remove(&key);
            self.blocking.cancel(key);
        }
        release_mcp_stream(connection, self.client.as_deref());
    }

    fn wake_fd(&self) -> Option<RawFd> {
        Some(self.notify.as_raw_fd())
    }
}

impl ServerHandler {
    /// Hand the stream handler's one pending job to the blocking pool. The
    /// job's value comes back as `StreamJobDone`; the pool's own completion
    /// only frees the key. A full queue is reported to the handler, which can
    /// answer with a protocol error instead of dropping the connection.
    fn service_stream_job(&mut self, connection: &mut Connection) {
        let id = connection.id;
        let job = match &mut connection.state {
            ConnState::Stream(driver) => driver.take_job(),
            _ => return,
        };
        let Some(job) = job else {
            return;
        };
        let key = self.next_stream_job;
        self.next_stream_job = self.next_stream_job.wrapping_add(1).max(1 << 32);
        let results = self.results_tx.clone();
        let notify = self.notify.try_clone().ok();
        let submitted = self.blocking.try_submit(key, move |cancel| {
            if cancel.is_cancelled() {
                return;
            }
            let outcome = job();
            let _ = results.send(WorkResult::StreamJobDone {
                connection: id,
                outcome,
            });
            if let Some(notify) = &notify {
                let _ = (&*notify).write(&[1]);
            }
        });
        match submitted {
            Ok(()) => {
                self.stream_jobs.insert(id, key);
                self.stream_job_keys.insert(key);
            }
            Err(_) => {
                if let ConnState::Stream(driver) = &mut connection.state {
                    driver.on_blocking_done(
                        BlockingOutcome::Failed("the blocking queue is full".into()),
                        &mut connection.out,
                        &mut connection.out_at,
                    );
                }
            }
        }
    }

    /// The plugin UI path: decide the route before the body is read, resolve
    /// the principal once, then attach the sans-IO handler to the connection.
    /// Returns \`None\` when this request is not a plugin UI route.
    fn plugin_ui_stream(&mut self, connection: &mut Connection) -> Option<PreRequest> {
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
        let exchange = match crate::face::plugin_ui::PluginUiFace::exchange(
            &head,
            connection.peer,
            &self.lan,
            self.config.clone(),
            Some(self.raw_hub.clone()),
        ) {
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
                write_response(
                    connection,
                    crate::server::http::stream_busy_response(),
                    false,
                );
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
        // A WebSocket client may have sent its first frames with the opening
        // handshake; the handler can submit a blocking job from those bytes.
        self.service_stream_job(connection);
        Some(PreRequest::Handled)
    }

    /// Run the face-owned part of one request. Inline routes answer now;
    /// blocking routes are submitted to the pool and the connection waits in
    /// `Processing`. Returns false when the request stays on the legacy path.
    fn serve_face(
        &mut self,
        connection: &mut Connection,
        request: &HttpRequest,
        keep_alive: bool,
    ) -> bool {
        let prepared = match super::prepare_face_request(
            self.face.as_ref(),
            request,
            connection.peer,
            &self.lan,
        ) {
            Ok(Some(prepared)) => prepared,
            Ok(None) => return false,
            Err(response) => {
                write_response(
                    connection,
                    if keep_alive {
                        keep_alive_response(response)
                    } else {
                        response
                    },
                    keep_alive,
                );
                return true;
            }
        };

        match prepared.route.dispatch {
            Dispatch::Inline => {
                let response = super::render_exchange(dispatch_inline(
                    self.face.as_ref(),
                    &prepared.route,
                    prepared.request,
                ));
                write_response(
                    connection,
                    if keep_alive {
                        keep_alive_response(response)
                    } else {
                        response
                    },
                    keep_alive,
                );
            }
            Dispatch::Blocking => {
                let endpoint = format!("{} {}", request.method, request.path);
                let (slot, done) = CompletionSlot::new(endpoint);
                let face = self.face.clone();
                let face_request = prepared.request;
                let job = move |cancel: CancelToken| {
                    if cancel.is_cancelled() {
                        return;
                    }
                    face.handle(face_request, done);
                };
                match self.blocking.try_submit(connection.id, job) {
                    Ok(()) => {
                        self.blocking_pending.insert(
                            connection.id,
                            PendingBlocking {
                                slot,
                                body: prepared.route.body,
                                keep_alive,
                            },
                        );
                        connection.state = ConnState::Processing;
                    }
                    Err(busy) => {
                        // Pipelining is unsupported, so the loop normally
                        // cannot submit a second job for one connection. The
                        // pool still enforces one-per-connection for future
                        // engines and stream handlers.
                        write_response(connection, blocking_busy(busy), false);
                    }
                }
            }
        }
        true
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

/// Deliver a stream job's value to its attached handler.
fn deliver_stream_outcome(connection: &mut Connection, outcome: BlockingOutcome) -> bool {
    let Connection {
        state, out, out_at, ..
    } = connection;
    let ConnState::Stream(driver) = state else {
        return true;
    };
    driver.on_blocking_done(outcome, out, out_at)
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

fn write_response(connection: &mut Connection, response: Vec<u8>, keep_alive: bool) {
    connection.out = response;
    connection.out_at = 0;
    connection.keep_alive = keep_alive;
    connection.state = ConnState::Writing;
}

fn busy_response(message: &str) -> Vec<u8> {
    let body = json!({ "error": { "code": "busy", "message": message } });
    http::response_with_headers(
        503,
        "application/json",
        &[("Retry-After".to_string(), "1".to_string())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn bridge_busy() -> Vec<u8> {
    busy_response("legacy request queue is full")
}

fn blocking_busy(busy: Busy) -> Vec<u8> {
    busy_response(match busy {
        Busy::QueueFull => "blocking request queue is full",
        Busy::AlreadyInFlight => "this connection already has a blocking request in flight",
        Busy::Closed => "server is shutting down",
    })
}

/// Whether the peer sent FIN, without consuming data already buffered in the
/// socket (that data is the next pipelined request). Called only for a
/// `Processing` connection; the normal read path detects EOF itself.
fn peer_closed(connection: &Connection) -> bool {
    socket_closed(connection.stream.as_raw_fd())
}

fn socket_closed(fd: RawFd) -> bool {
    let mut byte = 0u8;
    let read = unsafe {
        libc::recv(
            fd,
            &mut byte as *mut u8 as *mut libc::c_void,
            1,
            libc::MSG_PEEK,
        )
    };
    if read == 0 {
        return true;
    }
    if read < 0 {
        let error = std::io::Error::last_os_error();
        return error.kind() != std::io::ErrorKind::WouldBlock
            && error.kind() != std::io::ErrorKind::Interrupted;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_graceful_peer_close_is_detected_without_consuming_bytes() {
        let (mut reader, mut writer) = UnixStream::pair().expect("pair");
        reader.set_nonblocking(true).expect("nonblocking");
        assert!(!socket_closed(reader.as_raw_fd()));

        writer.write_all(b"next").expect("write");
        assert!(
            !socket_closed(reader.as_raw_fd()),
            "pending data is not a close"
        );
        let mut buffered = [0u8; 4];
        assert_eq!(reader.read(&mut buffered).expect("read"), 4);
        assert_eq!(&buffered, b"next", "MSG_PEEK must not consume the bytes");

        drop(writer);
        reader.set_nonblocking(false).expect("blocking");
        let mut eof = [0u8; 1];
        assert_eq!(reader.read(&mut eof).expect("eof"), 0);
        reader.set_nonblocking(true).expect("nonblocking");
        assert!(socket_closed(reader.as_raw_fd()));
    }

    #[test]
    fn every_busy_outcome_answers_503_with_retry_after() {
        for (busy, message) in [
            (Busy::QueueFull, "blocking request queue is full"),
            (
                Busy::AlreadyInFlight,
                "this connection already has a blocking request in flight",
            ),
            (Busy::Closed, "server is shutting down"),
        ] {
            let response = blocking_busy(busy);
            let text = String::from_utf8_lossy(&response);
            assert!(
                text.starts_with("HTTP/1.1 503 Service Unavailable"),
                "{text}"
            );
            assert!(text.contains("Retry-After: 1"), "{text}");
            assert!(text.contains("\"code\":\"busy\""), "{text}");
            assert!(text.contains(message), "{text}");
        }
    }

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
