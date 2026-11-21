//! The Unix server loop state machine.
//!
//! The loop in `server::http::engine` owns the listener, the sockets and the
//! request buffers. This module owns the connection state, the face dispatch,
//! the blocking-pool completions and the pre-body stream hooks. There is no
//! per-request worker: blocking work goes to the shared pool.

mod conns;
mod poll;

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener};
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
use crate::server::mcp::progress::McpSseStream;
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
    install_bus_wake: Option<crate::server::http::conn::BusWakeInstall>,
    governor: Option<Arc<crate::server::http::governor::CpuGovernor>>,
) -> std::io::Result<LoopHandle> {
    let (results_tx, results_rx) = mpsc::channel();
    let (notify_rx, notify_tx) = UnixStream::pair()?;
    let _ = notify_rx.set_nonblocking(true);
    let _ = notify_tx.set_nonblocking(true);
    let notify_tx = Arc::new(notify_tx);
    let loop_handle = LoopHandle::new(notify_tx.clone());
    let stop = loop_handle.stop_flag();
    // A published UI event writes one byte here, so the loop services the SSE
    // handler on its next iteration instead of waiting for the handler's poll
    // deadline (up to 100 ms). The pipe is non-blocking: a full pipe already
    // means a wake-up is pending.
    let bus_wake = Arc::new(AtomicBool::new(false));
    if let Some(install) = install_bus_wake {
        let notify = notify_tx.clone();
        let bus_wake = bus_wake.clone();
        install(Arc::new(move || {
            bus_wake.store(true, Ordering::SeqCst);
            let _ = (&*notify).write(&[1]);
        }));
    }
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
            config.limits.blocking_queue,
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
                bus_wake,
                governor,
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
    /// The final response is written and the write half is shut down; keep
    /// draining the peer's bytes until its FIN or the grace deadline, then
    /// close. Closing a socket with unread data makes the stack send RST,
    /// which can discard a status line the client has not read yet (#106).
    Draining(Instant),
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
    /// Set by the UI bus wake-up. The loop then re-checks every stream
    /// handler as if it were writable, instead of waiting for the handler's
    /// own poll deadline (which would hold a published event for one cadence).
    bus_wake: Arc<AtomicBool>,
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
    /// The CPU budget; absent when the document does not set one.
    governor: Option<Arc<crate::server::http::governor::CpuGovernor>>,
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
