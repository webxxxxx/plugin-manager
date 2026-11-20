//! The Unix server loop state machine.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module owns the connection state, the face dispatch,
//! the blocking-pool completions and the pre-body stream hooks. There is no
//! per-request worker: blocking work goes to the shared pool.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::platform::host::wake::Wake;
use crate::platform::log;
use crate::server::auth::lan::Lan;
use crate::server::control::client::StreamingClient;
use crate::server::control::handle::LoopHandle;
use crate::server::control::listener::{ControlServer, Source};
use crate::server::http::budget::{StreamBudget, StreamPermit};
use crate::server::http::driver::{StreamAdmission, StreamDriver};
use crate::server::http::engine::{
    self, keep_alive_response, wants_keep_alive, Connection as EngineConnection, ConnectionOps,
    ExtraOps, HttpRequest, LifecycleOps, PreRequest, Table, TimerOps,
};
use crate::server::http::model as http;
use crate::server::http::{
    attach_face_body, busy_response, complete_blocking, dispatch_inline, render_exchange,
    route_face_request, BlockingOutcome, BlockingPool, BodyPolicy, Busy, CancelToken,
    CompletionSlot, Dispatch, Exchange, Face, JobBudget, JobOutcome, PreparedFace, StreamClass,
    StreamHandler,
};
use crate::server::mcp::progress::{McpSseStream, PROGRESS_QUEUE_SLOTS};
use crate::wire::control::channel::PendingCall;

use crate::server::http::hooks::{FallbackHandler, PreBodyRoutes};

enum WorkResult {
    BlockingDone {
        key: u64,
        outcome: JobOutcome,
    },
    /// One blocking-pool job produced already-rendered response bytes.
    Rendered {
        connection: u64,
        response: Vec<u8>,
        keep_alive: bool,
    },
    StreamJobDone {
        connection: u64,
        outcome: BlockingOutcome,
    },
}

/// One queued or running blocking request. The server owns the completion slot
/// and the connection; the pool only reports that the job finished.
struct PendingBlocking {
    slot: CompletionSlot,
    body: BodyPolicy,
    keep_alive: bool,
}

pub(crate) fn start(
    http: Option<TcpListener>,
    control_user: UnixListener,
    control_admin: UnixListener,
    pending: mpsc::SyncSender<PendingCall>,
    wake: Arc<Wake>,
    lan: Arc<Mutex<Lan>>,
    config: Arc<crate::platform::config::Config>,
    raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    face: Arc<dyn Face>,
    routes: Arc<dyn PreBodyRoutes>,
    fallback: FallbackHandler,
    budget: StreamBudget,
) -> std::io::Result<LoopHandle> {
    let (results_tx, results_rx) = mpsc::channel();
    let (notify_rx, notify_tx) = UnixStream::pair()?;
    let _ = notify_rx.set_nonblocking(true);
    let _ = notify_tx.set_nonblocking(true);
    let notify_tx = Arc::new(notify_tx);
    let loop_handle = LoopHandle::new(notify_tx.clone());
    let stop = loop_handle.stop_flag();
    let control = ControlServer::new(
        control_user,
        control_admin,
        pending,
        wake,
        notify_tx.clone(),
    )?;

    // The pool belongs to the server loop, not to one request. Starting it
    // before the loop means a thread-spawn failure fails HTTP startup instead
    // of silently disabling every route after start().
    let blocking = {
        let results = results_tx.clone();
        let notify = notify_tx.try_clone()?;
        BlockingPool::start(
            config.limits.blocking_threads,
            crate::server::http::pool::QUEUE_LIMIT,
            crate::platform::host::threads::transfer_stack(&config.limits),
            move |key, outcome| {
                let _ = results.send(WorkResult::BlockingDone { key, outcome });
                let _ = (&notify).write(&[1]);
            },
        )?
    };

    let loop_limits = config.limits.clone();
    let join = thread::Builder::new()
        .name("svcd-server-loop".into())
        .stack_size(crate::platform::host::threads::transfer_stack(&loop_limits))
        .spawn(move || {
            let mut server = Server {
                lan,
                config,
                raw_hub,
                control,
                client: StreamingClient::new(),
                extra_keys: Vec::new(),
                stop,
                results: results_rx,
                results_tx: results_tx.clone(),
                notify: notify_rx,
                face,
                routes,
                fallback,
                budget,
                face_plans: HashMap::new(),
                blocking,
                blocking_pending: HashMap::new(),
                stream_jobs: HashMap::new(),
                stream_job_keys: HashSet::new(),
                next_stream_job: 1 << 32,
            };
            engine::run(http, &mut server, &loop_limits);
        })?;
    loop_handle.set_join(join);
    Ok(loop_handle)
}

enum ConnState {
    Reading,
    Processing,
    Writing,
    /// A sans-IO handler plus the server-side deadline, file send and
    /// `max_streams` slot (see `server::http::driver`).
    Stream(crate::server::http::driver::StreamDriver),
}

type Connection = EngineConnection<ConnState>;

/// One extra poll source in the order the engine dispatched it.
#[derive(Clone, Copy)]
enum ExtraSource {
    Control(Source),
    Client(u64),
}

struct Server {
    lan: Arc<Mutex<Lan>>,
    config: Arc<crate::platform::config::Config>,
    raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    control: ControlServer,
    client: StreamingClient,
    extra_keys: Vec<ExtraSource>,
    stop: Arc<AtomicBool>,
    results: Receiver<WorkResult>,
    /// A sender kept for stream-handler blocking jobs; their result is
    /// delivered to the handler, not to a completion slot.
    results_tx: Sender<WorkResult>,
    notify: UnixStream,
    face: Arc<dyn Face>,
    routes: Arc<dyn PreBodyRoutes>,
    fallback: FallbackHandler,
    /// The one long-lived stream budget built by the composition root.
    budget: StreamBudget,
    /// The route and principal resolved before this request's body was read.
    /// Present only for requests the face claims; the server-owned routes use
    /// `routes.body_policy`.
    face_plans: HashMap<u64, PreparedFace>,
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

impl ConnectionOps for Server {
    type State = ConnState;

    fn new_state(&mut self, _id: u64, _peer: IpAddr) -> ConnState {
        ConnState::Reading
    }

    fn on_accept(&mut self, _connection: &mut Connection) {}

    fn route_request(
        &mut self,
        id: u64,
        peer: IpAddr,
        head: &http::Head,
    ) -> Result<BodyPolicy, Vec<u8>> {
        match route_face_request(self.face.as_ref(), head, peer, &self.lan) {
            Ok(Some(prepared)) => {
                let body = prepared.route.body;
                self.face_plans.insert(id, prepared);
                Ok(body)
            }
            Ok(None) => Ok(self.routes.body_policy(head)),
            Err(response) => Err(response),
        }
    }

    fn reads_request(&self, connection: &Connection) -> bool {
        matches!(connection.state, ConnState::Reading)
    }

    fn pre_request(&mut self, connection: &mut Connection) -> PreRequest {
        if connection.pending.is_some() {
            return PreRequest::Continue;
        }
        let Some(header_end) = http::header_end(&connection.buffer) else {
            return PreRequest::Continue;
        };
        if header_end > self.config.limits.max_header {
            return PreRequest::Continue;
        }
        let Some((head, body_start)) = http::parse_head(&connection.buffer[..header_end + 4])
        else {
            return PreRequest::Continue;
        };
        // A pre-body route can answer before advance_request sees the head, so
        // record the method here as well. advance_request resets it for the
        // requests that continue to the buffered path.
        connection.head_only = http::is_head(&head.method);
        let claim = self
            .routes
            .plugin_ui(&head, connection.peer, &self.lan, self.raw_hub.clone())
            .or_else(|| self.routes.package_upload(&head, &self.lan))
            .or_else(|| self.routes.blob_upload(&head, &self.lan));
        let Some(claim) = claim else {
            return PreRequest::Continue;
        };
        let exchange = match claim {
            Ok(exchange) => exchange,
            Err(response) => {
                write_response(connection, response, false);
                return PreRequest::Handled;
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
            return PreRequest::Handled;
        };
        let initial = connection.buffer.split_off(body_start);
        connection.buffer.clear();
        attach_stream(
            connection,
            handler,
            &initial,
            None,
            &self.budget,
            &self.config.limits,
        );
        self.service_stream_job(connection);
        PreRequest::Handled
    }

    fn on_request(&mut self, connection: &mut Connection, request: HttpRequest) {
        let keep_alive = wants_keep_alive(&request);
        if let Some(prepared) = self.face_plans.remove(&connection.id) {
            self.serve_face(connection, prepared, request.body, keep_alive);
            return;
        }
        if let Some(prepared) = self.routes.mcp(&request, &self.lan) {
            match prepared {
                Ok(call) => {
                    // Reserve the long-lived slot before starting the plugin
                    // call: a full budget must refuse the request without a
                    // side effect on the service.
                    let Ok(Some(permit)) = self
                        .budget
                        .acquire(StreamClass::LongLived, self.config.limits.max_streams)
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
                    // A token/connect failure becomes the stream's first
                    // progress event, so the client still sees the -32603
                    // channel error instead of an HTTP 500 before the SSE
                    // headers.
                    let stream = self.client.open(
                        self.config.paths.control_dir(),
                        "registry.call",
                        request,
                        sender,
                    );
                    let cancel = self.client.canceller();
                    let handler = McpSseStream::new(
                        receiver,
                        call.id,
                        stream,
                        Box::new(move |id| cancel.cancel(id)),
                    );
                    // The slot reserved above becomes the driver's
                    // `max_streams` permit.
                    attach_stream(
                        connection,
                        Box::new(handler),
                        &[],
                        Some(permit),
                        &self.budget,
                        &self.config.limits,
                    );
                    return;
                }
                Err(response) => {
                    write_response(connection, response, false);
                    return;
                }
            }
        }
        self.submit_rendered(connection, request, keep_alive);
    }

    fn on_error(&mut self, connection: &mut Connection, response: Vec<u8>) {
        if response.is_empty() {
            // An interim response could not be written atomically. Close
            // without adding a status line behind the truncated bytes.
            connection.out.clear();
            connection.out_at = 0;
            connection.keep_alive = false;
            connection.state = ConnState::Writing;
            return;
        }
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
        true
    }

    fn on_writable(&mut self, connection: &mut Connection) -> bool {
        if matches!(&connection.state, ConnState::Stream(_)) {
            let keep = stream_writable(connection);
            if keep {
                self.service_stream_job(connection);
            }
            return keep;
        }
        {
            {
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

    fn interest(&self, connection: &Connection) -> i16 {
        let mut events = match &connection.state {
            ConnState::Reading => libc::POLLIN,
            ConnState::Writing => libc::POLLOUT,
            ConnState::Stream(driver) => {
                if driver.wants_read() {
                    libc::POLLIN
                } else if driver.has_job() {
                    // A stream job in flight still has to notice a client FIN
                    // so it can be cancelled; the engine owns the platform's
                    // half-close flag. A stream merely blocked on write keeps
                    // the old behaviour and may still deliver its response.
                    engine::paused_stream_interest()
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
        // A plan is resolved per request and consumed by `serve_face`; a
        // connection that dies while waiting for its body leaves it behind.
        self.face_plans.remove(&connection.id);
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
    }
}

impl TimerOps for Server {
    type State = ConnState;
    fn on_tick(&mut self, connections: &mut Table<ConnState>) {
        let mut drain = [0u8; 64];
        while self.notify.read(&mut drain).is_ok() {}
        // The same wake pipe carries blocking-pool completions, control
        // replies, and outbound progress retries.
        self.control.drain_replies();
        self.control.flush();
        self.client.drain_cancels();
        self.client.tick();

        while let Ok(result) = self.results.try_recv() {
            match result {
                WorkResult::Rendered {
                    connection,
                    response,
                    keep_alive,
                } => {
                    // The pool's own BlockingDone clears stream_job_keys; this
                    // map only tracks the connection's one in-flight job.
                    self.stream_jobs.remove(&connection);
                    if let Some(connection) = connections.get_mut(&connection) {
                        write_response(connection, response, keep_alive);
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
                                render_exchange(complete_blocking(pending.body, exchange));
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
                    // The job is no longer in flight. A follow-up job submitted
                    // from service_stream_job below gets a fresh key.
                    self.stream_jobs.remove(&id);
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
    }
}

impl ExtraOps for Server {
    fn wake_fd(&self) -> Option<RawFd> {
        Some(self.notify.as_raw_fd())
    }

    fn prepare_extra(&mut self, out: &mut Vec<(RawFd, i16)>) {
        self.extra_keys.clear();
        let mut control_keys = Vec::new();
        self.control.poll_sources(out, &mut control_keys);
        for key in control_keys {
            self.extra_keys.push(ExtraSource::Control(key));
        }
        let mut client_keys = Vec::new();
        self.client.poll_sources(out, &mut client_keys);
        for id in client_keys {
            self.extra_keys.push(ExtraSource::Client(id));
        }
    }

    fn on_extra_ready(&mut self, index: usize, revents: i16) {
        match self.extra_keys.get(index).copied() {
            Some(ExtraSource::Control(source)) => self.control.ready(source, revents),
            Some(ExtraSource::Client(id)) => self.client.ready(id, revents),
            None => {}
        }
    }
}

impl LifecycleOps for Server {
    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn on_stop(&mut self) {
        self.control.drain_for_shutdown();
        self.client.close();
    }
}

impl Server {
    /// Run one server-owned buffered route on the blocking pool. The closure
    /// is the composition root's leftover router; the loop only writes the
    /// rendered bytes back.
    fn submit_rendered(
        &mut self,
        connection: &mut Connection,
        request: HttpRequest,
        keep_alive: bool,
    ) {
        let id = connection.id;
        let key = self.next_job_key();
        let submitted = submit_rendered_job(
            &self.blocking,
            id,
            key,
            self.fallback.clone(),
            self.lan.clone(),
            self.results_tx.clone(),
            self.notify.try_clone().ok(),
            request,
            keep_alive,
        );
        match submitted {
            Ok(()) => {
                debug_assert!(
                    !self.stream_jobs.contains_key(&id),
                    "one in-flight blocking job per connection"
                );
                self.stream_jobs.insert(id, key);
                self.stream_job_keys.insert(key);
                connection.state = ConnState::Processing;
            }
            Err(busy) => write_response(connection, blocking_busy(busy), false),
        }
    }

    /// Allocate one detached blocking-pool key. Face jobs and server-owned
    /// routes share the range so a pool completion can never collide with a
    /// connection id.
    fn next_job_key(&mut self) -> u64 {
        let key = self.next_stream_job;
        self.next_stream_job = self.next_stream_job.wrapping_add(1).max(1 << 32);
        key
    }

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
        let key = self.next_job_key();
        let results = self.results_tx.clone();
        let notify = self.notify.try_clone().ok();
        // Stamp the deadline at submission, the same instant the driver starts
        // its no-progress clock. A job that waits in the queue must not get a
        // fresh budget after the connection has already been given up on.
        let deadline = Instant::now() + self.config.limits.transfer_timeout;
        let submitted = self.blocking.try_submit(key, move |cancel| {
            if cancel.is_cancelled() {
                return;
            }
            let outcome = job(JobBudget::at(cancel, deadline));
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
                debug_assert!(
                    !self.stream_jobs.contains_key(&id),
                    "one in-flight blocking job per connection"
                );
                self.stream_jobs.insert(id, key);
                self.stream_job_keys.insert(key);
            }
            Err(_) => {
                if let ConnState::Stream(driver) = &mut connection.state {
                    driver.on_blocking_done(
                        BlockingOutcome::Busy,
                        &mut connection.out,
                        &mut connection.out_at,
                    );
                }
            }
        }
    }

    /// Run the face-owned part of one request from the plan resolved before the
    /// body was read. Inline routes answer now; blocking routes are submitted to
    /// the pool and the connection waits in `Processing`.
    fn serve_face(
        &mut self,
        connection: &mut Connection,
        prepared: PreparedFace,
        body: Vec<u8>,
        keep_alive: bool,
    ) {
        let prepared = match attach_face_body(prepared, &body) {
            Ok(prepared) => prepared,
            Err(response) => {
                // The wire limit should have caught this before the body was
                // read. A late mismatch must not keep the connection alive,
                // because unread body bytes would precede the next request.
                write_response(connection, response, false);
                return;
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
                        attach_stream(
                            connection,
                            handler,
                            &[],
                            None,
                            &self.budget,
                            &self.config.limits,
                        );
                        self.service_stream_job(connection);
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
                let endpoint = format!("{} {}", prepared.request.method, prepared.request.path);
                let (slot, done) = CompletionSlot::new(endpoint);
                let face = self.face.clone();
                let face_request = prepared.request;
                let control_timeout = self.config.limits.control_timeout;
                let job = move |cancel: CancelToken| {
                    if cancel.is_cancelled() {
                        return;
                    }
                    // Stamp the budget when the job actually starts, not when
                    // it was queued: time spent behind another job is not part
                    // of this route's control round trip.
                    let request = face_request.with_control_budget(
                        Instant::now(),
                        control_timeout,
                        Some(cancel),
                    );
                    face.handle(request, done);
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
    budget: &StreamBudget,
    limits: &std::sync::Arc<crate::platform::limits::Limits>,
) {
    connection.keep_alive = false;
    let mut driver = match reserved {
        Some(permit) => StreamDriver::admit_reserved(handler, permit, limits.clone()),
        None => match StreamDriver::admit(handler, budget, limits.max_streams, limits.clone()) {
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

/// Write one already-rendered response. The connection carries the method of
/// the request being answered: a HEAD response keeps its head (and therefore
/// GET's `Content-Length`) but never its body, which would otherwise be read
/// as the start of the next response on a keep-alive connection.
fn write_response(connection: &mut Connection, response: Vec<u8>, keep_alive: bool) {
    connection.out = if connection.head_only {
        http::strip_body(response)
    } else {
        response
    };
    connection.out_at = 0;
    connection.keep_alive = keep_alive;
    connection.state = ConnState::Writing;
}

fn blocking_busy(busy: Busy) -> Vec<u8> {
    busy_response(match busy {
        Busy::QueueFull => "blocking request queue is full",
        Busy::AlreadyInFlight => "this connection already has a blocking request in flight",
        Busy::Closed => "server is shutting down",
    })
}

/// Submit one server-owned buffered route. Kept as a free function so a
/// queue-full test can drive it without building a whole connection table.
fn submit_rendered_job(
    pool: &BlockingPool,
    connection: u64,
    key: u64,
    fallback: FallbackHandler,
    lan: Arc<Mutex<Lan>>,
    results: Sender<WorkResult>,
    notify: Option<UnixStream>,
    request: HttpRequest,
    keep_alive: bool,
) -> Result<(), Busy> {
    pool.try_submit(key, move |cancel| {
        if cancel.is_cancelled() {
            return;
        }
        let response = fallback(&request, &lan);
        let response = if keep_alive {
            keep_alive_response(response)
        } else {
            response
        };
        let _ = results.send(WorkResult::Rendered {
            connection,
            response,
            keep_alive,
        });
        if let Some(notify) = &notify {
            let _ = (&*notify).write(&[1]);
        }
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
    // SAFETY: The fd comes from a live connection/listener owned by the loop; the pollfd array is local to the call.
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
    use std::io::{Read, Write};

    #[test]
    fn a_full_blocking_pool_refuses_a_server_owned_route() {
        let (results, _results_rx) = mpsc::channel();
        let pool = BlockingPool::start(1, 1, 256 * 1024, |_, _| {}).expect("pool");
        let (release, wait) = mpsc::channel::<()>();
        let (started, running) = mpsc::channel::<()>();
        pool.try_submit(1, move |_| {
            let _ = started.send(());
            let _ = wait.recv();
        })
        .expect("running job");
        running.recv().expect("worker started");
        pool.try_submit(2, |_| {}).expect("queued job");

        let bind: std::net::SocketAddr = "127.0.0.1:0".parse().expect("bind");
        let lan = Arc::new(Mutex::new(Lan::new(bind, bind, false, None)));
        let (notify, _peer) = UnixStream::pair().expect("pair");
        let fallback: FallbackHandler =
            Arc::new(|_, _| http::json_error(500, "internal", "unused"));
        let request = HttpRequest {
            method: "GET".into(),
            path: "/api/v1/blobs".into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        let busy = submit_rendered_job(
            &pool,
            7,
            3,
            fallback,
            lan,
            results,
            Some(notify),
            request,
            false,
        )
        .expect_err("a full pool must refuse the route");
        assert_eq!(busy, Busy::QueueFull);
        let _ = release.send(());
    }

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
    fn a_head_write_drops_the_blocking_busy_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let stream =
            std::net::TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let mut connection = Connection {
            id: 1,
            stream,
            peer: "127.0.0.1".parse().expect("peer"),
            last_active: Instant::now(),
            buffer: Vec::new(),
            out: Vec::new(),
            out_at: 0,
            keep_alive: false,
            head_only: true,
            pending: None,
            state: ConnState::Reading,
        };
        let full = blocking_busy(Busy::QueueFull);
        let full_text = String::from_utf8(full.clone()).expect("utf8");
        let content_length = full_text
            .lines()
            .find(|line| line.starts_with("Content-Length: "))
            .expect("content length")
            .to_string();
        write_response(&mut connection, full, false);
        let text = String::from_utf8(connection.out).expect("utf8");
        let header_end = text.find("\r\n\r\n").expect("header end") + 4;
        assert_eq!(text.len(), header_end, "the HEAD body must be gone: {text}");
        assert!(
            text.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{text}"
        );
        assert!(text.contains("Retry-After: 1"), "{text}");
        assert!(
            text.contains(&content_length),
            "Content-Length still describes GET's body: {text}"
        );
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
}
