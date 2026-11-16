//! Temporary legacy bridge for the server loop.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module keeps the endpoint state machine and the old
//! worker/takeover path alive until #115-#125 move them to the face. The queue
//! is bounded; a full queue answers 503 instead of growing without limit.

use std::collections::{HashMap, HashSet};
use std::os::unix::io::{AsRawFd, RawFd};

use crate::server::http::budget::StreamPermit;
use crate::server::http::driver::{StreamAdmission, StreamDriver};
use crate::server::http::engine::{self, HttpRequest, LegacyBridge, PreRequest, Table};
use crate::server::http::pool::PoolJob;
use crate::server::http::StreamClass;
use crate::server::http::{
    complete_blocking, dispatch_inline, BlockingJob, BlockingOutcome, BlockingPool, BodyPolicy,
    Busy, CancelToken, CompletionSlot, Dispatch, Exchange, Face, JobOutcome, StreamHandler,
};
use crate::server::mcp::progress::{McpSseStream, PROGRESS_QUEUE_SLOTS};

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

    let face: Arc<dyn Face> = Arc::new(crate::face::api::ApiFace::new(config.clone(), lan.clone()));
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
                notify: notify_rx,
                face,
                blocking,
                blocking_pending: HashMap::new(),
                stream_jobs: HashMap::new(),
                stream_job_deferred: HashSet::new(),
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
    face: Arc<dyn Face>,
    blocking: BlockingPool,
    blocking_pending: HashMap<u64, PendingBlocking>,
    /// One in-flight blocking job per stream connection (an upload chunk or a
    /// finalize). Completion resumes the handler; disconnect cancels it.
    stream_jobs: HashMap<u64, mpsc::Receiver<BlockingOutcome>>,
    /// Connections already logged as paused by a full blocking queue; cleared
    /// when the job is accepted or the connection closes.
    stream_job_deferred: HashSet<u64>,
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
        if let Some(outcome) = self.blob_upload_stream(connection) {
            return outcome;
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
                    let Some(client) = &self.client else {
                        write_response(
                            connection,
                            http::json_error(
                                500,
                                "stream_unavailable",
                                "management client reactor is unavailable",
                            ),
                            false,
                        );
                        return;
                    };
                    // Reserve the long-lived slot before starting the plugin
                    // call: a full budget must refuse the request without a
                    // side effect on the service.
                    let Ok(Some(permit)) = crate::server::http::budget::StreamBudget::global()
                        .acquire(StreamClass::LongLived, crate::limits::current().max_streams)
                    else {
                        write_response(
                            connection,
                            crate::server::http::stream_busy_response(),
                            false,
                        );
                        return;
                    };
                    let (sender, receiver) = mpsc::sync_channel(PROGRESS_QUEUE_SLOTS);
                    let request = json!({
                        "name": call.name,
                        "arguments": call.arguments,
                        "progressToken": call.progress_token,
                    });
                    match client.open(&self.config, "registry.call", request, sender) {
                        Ok(stream) => {
                            let cancel_client = client.clone();
                            let handler = McpSseStream::new(
                                receiver,
                                call.id,
                                stream,
                                Box::new(move |id| cancel_client.cancel(id)),
                            );
                            // The slot reserved above becomes the driver's
                            // `max_streams` permit.
                            attach_stream(connection, Box::new(handler), &[], Some(permit));
                            self.submit_stream_job(connection);
                        }
                        Err(error) => write_response(
                            connection,
                            http::json_error(
                                500,
                                "stream_start_failed",
                                &format!("cannot start streaming call: {}", error),
                            ),
                            false,
                        ),
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
            let keep = stream_readable(connection);
            if keep {
                self.submit_stream_job(connection);
            }
            return keep;
        }
        match &connection.state {
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
            let keep = stream_writable(connection);
            if keep {
                self.submit_stream_job(connection);
            }
            return keep;
        }
        match &connection.state {
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
                WorkResult::BlockingDone { key, outcome } => {
                    if let Some(receiver) = self.stream_jobs.remove(&key) {
                        // Finished must carry a result; a cancelled/dropped
                        // job means the connection or daemon is going away,
                        // so close it rather than inventing a handler failure.
                        let blocking_outcome = match outcome {
                            JobOutcome::Finished => {
                                Some(receiver.try_recv().unwrap_or_else(|_| {
                                    BlockingOutcome::Failed(
                                        "stream job finished without a result".into(),
                                    )
                                }))
                            }
                            JobOutcome::Cancelled | JobOutcome::Dropped => None,
                        };
                        let Some(blocking_outcome) = blocking_outcome else {
                            if let Some(mut connection) = connections.remove(&key) {
                                self.on_close(&mut connection);
                            }
                            continue;
                        };
                        if let Some(connection) = connections.get_mut(&key) {
                            let keep = {
                                let Connection {
                                    state, out, out_at, ..
                                } = connection;
                                match state {
                                    ConnState::Stream(driver) => {
                                        driver.on_blocking_done(blocking_outcome, out, out_at)
                                    }
                                    _ => true,
                                }
                            };
                            if keep {
                                self.submit_stream_job(connection);
                            } else if let Some(mut connection) = connections.remove(&key) {
                                self.on_close(&mut connection);
                            }
                        }
                        continue;
                    }
                    let Some(pending) = self.blocking_pending.remove(&key) else {
                        // Cancel/drop outcomes are expected after a disconnect:
                        // on_close already sealed and removed the slot. A
                        // Finished outcome with no pending entry would mean the
                        // loop lost its bookkeeping.
                        if outcome == JobOutcome::Finished && connections.contains_key(&key) {
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
                if let Some(mut connection) = connections.remove(&id) {
                    self.on_close(&mut connection);
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
                Some(connection) => {
                    let keep = stream_tick(connection, now);
                    if keep {
                        self.submit_stream_job(connection);
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
            if matches!(&connection.state, ConnState::Sse(_)) {
                fill_sse(connection);
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
            ConnState::Stream(_) if stream_wants_read(connection) => libc::POLLIN,
            ConnState::Stream(_) => 0,
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
        self.stream_jobs.remove(&connection.id);
        self.stream_job_deferred.remove(&connection.id);
        if let Some(pending) = self.blocking_pending.remove(&connection.id) {
            // Seal the slot first: a job that finishes after this point writes
            // into a sealed slot and is ignored. Then free a queued slot or
            // flip a running job's cancellation token.
            pending.slot.take();
        }
        // A queued stream job is dropped; a running one sees the token and
        // drops its result. Either way the connection key is released.
        self.blocking.cancel(connection.id);
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
    /// Claim a blob/file upload before its body is buffered, resolve auth and
    /// endpoint policy once, then attach the sans-IO upload handler. Any
    /// already-read body bytes are delivered as the first data event.
    fn blob_upload_stream(&mut self, connection: &mut Connection) -> Option<PreRequest> {
        if connection.pending.is_some() {
            return None;
        }
        let header_end = http::header_end(&connection.buffer)?;
        if header_end > crate::limits::current().max_header {
            return None;
        }
        let (head, body_start) = http::parse_head(&connection.buffer[..header_end + 4])?;
        crate::face::blob_upload::upload_target(&head)?;
        let exchange = match crate::face::blob_upload::exchange(&head, &self.lan) {
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
                    "a streamed upload route returned a memory response",
                ),
                false,
            );
            return Some(PreRequest::Handled);
        };
        let initial = connection.buffer.split_off(body_start);
        connection.buffer.clear();
        attach_stream(connection, handler, &initial, None);
        self.submit_stream_job(connection);
        Some(PreRequest::Handled)
    }

    /// Hand one pending stream job to the blocking pool. A full queue keeps
    /// the job in the driver and retries on the next tick, so an upload chunk
    /// is never lost; input stays paused while the job is pending or running.
    fn submit_stream_job(&mut self, connection: &mut Connection) {
        let Some(job) = take_stream_job(connection) else {
            return;
        };
        match submit_stream_job_to(&self.blocking, &mut self.stream_jobs, connection.id, job) {
            Ok(()) => {
                self.stream_job_deferred.remove(&connection.id);
                mark_stream_blocking(connection);
            }
            Err((busy, job)) => {
                // Queue full or already in flight: keep the job in the driver.
                // A later tick retries; input stays paused. The pause is bounded
                // by the driver's transfer deadline. Log it once per connection
                // so a saturated pool is visible rather than silent.
                restore_stream_job(connection, job);
                if busy != Busy::Closed && self.stream_job_deferred.insert(connection.id) {
                    log::svcd(&format!(
                        "svcd: upload paused: blocking pool queue is full (connection {})",
                        connection.id
                    ));
                }
                if busy == Busy::Closed {
                    log::svcd("svcd: stream job deferred: blocking pool is closing");
                }
            }
        }
    }

    /// The plugin UI asset path: decide the route before the body is read,
    /// resolve the principal once, then attach the sans-IO handler to the
    /// connection. Returns \`None\` when this request is not a static asset.
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
        attach_stream(connection, handler, &initial, None);
        self.submit_stream_job(connection);
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
                match dispatch_inline(self.face.as_ref(), &prepared.route, prepared.request) {
                    Ok(Exchange::Response(response)) => {
                        let response = if keep_alive {
                            keep_alive_response(response.into_bytes())
                        } else {
                            response.into_bytes()
                        };
                        write_response(connection, response, keep_alive);
                    }
                    Ok(Exchange::Stream(handler)) => {
                        // A streamed response is not a keep-alive response: its
                        // handler may own a file range and the wire head says
                        // `Connection: close`.
                        attach_stream(connection, handler, &[], None);
                        self.submit_stream_job(connection);
                    }
                    Err(cause) => {
                        let response = http::json_error(500, "internal", &cause.message());
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
                }
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

/// Attach a stream handler under `max_streams`. On Busy the connection gets
/// the shared 503 before any handler bytes are written. `reserved` carries a
/// slot acquired before a side effect (an outbound call was already opened);
/// `None` lets the driver acquire one now.
fn attach_stream(
    connection: &mut Connection,
    handler: Box<dyn StreamHandler>,
    initial: &[u8],
    reserved: Option<StreamPermit>,
) {
    connection.keep_alive = false;
    let mut driver = match reserved {
        Some(permit) => StreamDriver::admit_reserved(handler, permit),
        None => match StreamDriver::admit(handler, crate::limits::current().max_streams) {
            StreamAdmission::Admitted(driver) => driver,
            StreamAdmission::Busy => {
                write_response(
                    connection,
                    crate::server::http::stream_busy_response(),
                    false,
                );
                return;
            }
        },
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
        return;
    }
    if initial.is_empty() {
        return;
    }
    let keep = {
        let Connection {
            state, out, out_at, ..
        } = connection;
        match state {
            ConnState::Stream(driver) => driver.on_data(initial, out, out_at),
            _ => true,
        }
    };
    if !keep {
        finish_stream(connection);
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

type StreamJobs = HashMap<u64, mpsc::Receiver<BlockingOutcome>>;

/// Queue one stream job, handing it back when the pool refuses it. The caller
/// keeps it in the driver and retries; a refused chunk is never dropped.
fn submit_stream_job_to(
    pool: &BlockingPool,
    jobs: &mut StreamJobs,
    key: u64,
    job: BlockingJob,
) -> Result<(), (Busy, BlockingJob)> {
    let cell = Arc::new(Mutex::new(Some(job)));
    let job_cell = cell.clone();
    let (sender, receiver) = mpsc::channel();
    let work: PoolJob = Box::new(move |cancel| {
        if cancel.is_cancelled() {
            return;
        }
        let Some(job) = job_cell
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        else {
            return;
        };
        let outcome = job();
        let _ = sender.send(outcome);
    });
    match pool.try_submit_boxed(key, work) {
        Ok(()) => {
            jobs.insert(key, receiver);
            Ok(())
        }
        Err((busy, work)) => {
            drop(work);
            let job = cell
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("a refused job is still in its cell");
            Err((busy, job))
        }
    }
}

fn stream_has_file(connection: &Connection) -> bool {
    matches!(&connection.state, ConnState::Stream(driver) if driver.has_file())
}

fn stream_wants_read(connection: &Connection) -> bool {
    match &connection.state {
        ConnState::Stream(driver) => driver.wants_read(),
        _ => true,
    }
}

fn take_stream_job(connection: &mut Connection) -> Option<BlockingJob> {
    match &mut connection.state {
        ConnState::Stream(driver) => driver.take_blocking_job(),
        _ => None,
    }
}

fn restore_stream_job(connection: &mut Connection, job: BlockingJob) {
    if let ConnState::Stream(driver) = &mut connection.state {
        driver.restore_blocking_job(job);
    }
}

fn mark_stream_blocking(connection: &mut Connection) {
    if let ConnState::Stream(driver) = &mut connection.state {
        driver.begin_blocking();
    }
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

    #[test]
    fn a_full_stream_queue_hands_the_job_back_and_a_retry_runs_it() {
        use std::time::Duration;

        let (finished_tx, finished_rx) = mpsc::channel();
        let pool = BlockingPool::start(1, 1, move |key, outcome| {
            let _ = finished_tx.send((key, outcome));
        })
        .expect("pool");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        pool.try_submit(1, move |_| {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("worker job");
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker started");
        pool.try_submit(2, |_| {}).expect("queue is full");

        let mut jobs: StreamJobs = HashMap::new();
        let job: BlockingJob = Box::new(|| BlockingOutcome::Ok);
        let (busy, job) = submit_stream_job_to(&pool, &mut jobs, 3, job)
            .expect_err("a full queue refuses the chunk");
        assert_eq!(busy, Busy::QueueFull);
        assert!(jobs.is_empty(), "a refused job holds no completion slot");

        release_tx.send(()).expect("release worker");
        for _ in 0..2 {
            finished_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("queued jobs finish");
        }
        if submit_stream_job_to(&pool, &mut jobs, 3, job).is_err() {
            panic!("the queue has room after the worker drains");
        }
        let outcome = jobs
            .remove(&3)
            .expect("retry installed a completion slot")
            .recv_timeout(Duration::from_secs(5))
            .expect("retried job result");
        assert!(matches!(outcome, BlockingOutcome::Ok));
        let (key, result) = finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("retried job completion");
        assert_eq!(key, 3);
        assert_eq!(result, JobOutcome::Finished);
    }
}
