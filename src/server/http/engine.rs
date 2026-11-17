//! The HTTP server loop: it owns the listener, the accepted sockets, the
//! request buffers and the pending-request table. A `ServerLoop` implements
//! the per-connection state machine; the loop itself never knows an endpoint.
//!
//! The loop is Unix-only for now. Windows keeps the per-connection thread
//! fallback (ADR-0016); moving that to IOCP belongs to S3/S6.

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::net::{IpAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::time::{Duration, Instant};

use crate::http;

#[cfg(unix)]
use crate::server::http::contract::BodyPolicy;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};

/// Poll flags that mean the peer is gone. Linux reports a half-close as
/// `POLLRDHUP` even when the socket still has unread bytes; without it a
/// paused stream (a job in flight, so no read interest) never sees the client
/// FIN and cannot cancel that job.
#[cfg(all(unix, target_os = "linux"))]
const POLL_CLOSE: i16 = libc::POLLERR | libc::POLLHUP | libc::POLLRDHUP;
#[cfg(all(unix, not(target_os = "linux")))]
const POLL_CLOSE: i16 = libc::POLLERR | libc::POLLHUP;

/// What a paused stream asks poll for. It must not ask to read (the handler is
/// applying backpressure), but Linux only reports a half-close through
/// `POLLRDHUP`; other Unixes keep `POLLHUP`, which the loop already handles.
#[cfg(unix)]
pub(crate) fn paused_stream_interest() -> i16 {
    #[cfg(target_os = "linux")]
    {
        libc::POLLRDHUP
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// The busy response for a connection refused by the loop. Both engines send
/// the same bytes so a client sees one status on every host.
pub const BUSY_CONNECTION_RESPONSE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    Content-Length: 0\r\n\
    Connection: close\r\n\r\n";

/// An owned request head plus a complete buffered body.
#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[cfg(unix)]
pub struct PendingRequest {
    method: String,
    path: String,
    version: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    framing: PendingFraming,
}

#[cfg(unix)]
pub enum PendingFraming {
    None,
    Length(u64),
    Chunked(http::ChunkedDecoder),
}

/// One accepted connection. The loop owns the socket and the two buffers; the
/// handler owns only the protocol state `S`.
#[cfg(unix)]
pub struct Connection<S> {
    pub id: u64,
    pub stream: TcpStream,
    pub peer: IpAddr,
    pub last_active: Instant,
    pub buffer: Vec<u8>,
    pub out: Vec<u8>,
    pub out_at: usize,
    pub keep_alive: bool,
    /// The request being answered is HEAD: the server writes its response
    /// head and no body. Set when the request head is parsed and cleared
    /// before the next one, so a delayed completion still reads the method of
    /// the request it belongs to.
    pub head_only: bool,
    pub pending: Option<PendingRequest>,
    pub state: S,
}

#[cfg(unix)]
impl<S> Connection<S> {
    /// Flush the connection's output buffer. Returns false when the peer is
    /// gone. Responses and stream frames both use this one write path.
    pub fn write_out(&mut self) -> bool {
        if self.out_at >= self.out.len() {
            self.out.clear();
            self.out_at = 0;
            return true;
        }
        match self.stream.write(&self.out[self.out_at..]) {
            Ok(0) => false,
            Ok(written) => {
                self.out_at += written;
                self.last_active = Instant::now();
                if self.out_at >= self.out.len() {
                    self.out.clear();
                    self.out_at = 0;
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }
}

#[cfg(unix)]
pub type Table<S> = HashMap<u64, Connection<S>>;

/// What a handler wants the loop to do after it inspected the request head but
/// before the body is read. Stream handlers that consume the raw body need
/// this window.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreRequest {
    /// Not a stream route; let the loop parse and buffer the request.
    Continue,
    /// The handler took the readable bytes and set the connection's next
    /// state itself (a written response, or an attached stream driver); the
    /// loop must not parse the same buffer again.
    Handled,
}

/// The server's connection state machine. The loop owns the connection and the
/// buffers; this trait only lets the full HTTP server and the control-only
/// build share the same reactor. It is not a product seam.
#[cfg(unix)]
pub trait ServerLoop {
    type State;

    fn new_state(&mut self, id: u64, peer: IpAddr) -> Self::State;
    fn on_accept(&mut self, connection: &mut Connection<Self::State>);
    /// True when the state machine is waiting for an HTTP request head/body.
    fn reads_request(&self, connection: &Connection<Self::State>) -> bool;
    /// Resolve the request's route (once) and authentication, and return the
    /// body policy the loop must apply before reading a single body byte.
    fn route_request(
        &mut self,
        id: u64,
        peer: IpAddr,
        head: &http::Head,
    ) -> Result<BodyPolicy, Vec<u8>>;
    fn pre_request(&mut self, connection: &mut Connection<Self::State>) -> PreRequest;
    fn on_request(&mut self, connection: &mut Connection<Self::State>, request: HttpRequest);
    fn on_error(&mut self, connection: &mut Connection<Self::State>, response: Vec<u8>);
    fn on_readable(&mut self, connection: &mut Connection<Self::State>) -> bool;
    fn on_writable(&mut self, connection: &mut Connection<Self::State>) -> bool;
    fn on_tick(&mut self, table: &mut Table<Self::State>);
    fn interest(&self, connection: &Connection<Self::State>) -> i16;
    fn is_idle(&self, connection: &Connection<Self::State>, now: Instant, idle: Duration) -> bool;
    fn on_close(&mut self, connection: &mut Connection<Self::State>);

    /// The loop is about to drop a connection after `POLLERR` / `POLLHUP`
    /// (or Linux's `POLLRDHUP`). Handlers use this for protocol-specific
    /// diagnostics.
    fn on_poll_error(&mut self, connection: &mut Connection<Self::State>, _revents: i16) {
        let _ = connection;
    }

    /// The blocking pool and control client wake the loop through this fd; the
    /// loop only uses it to leave poll early, the handler drains it in `on_tick`.
    fn wake_fd(&self) -> Option<RawFd> {
        None
    }

    /// Append the server-loop's extra poll sources (the control listener,
    /// control connections, outbound management streams). The handler keeps
    /// the same order for `on_extra_ready`; indices are stable for one pass.
    fn prepare_extra(&mut self, _out: &mut Vec<(RawFd, i16)>) {}

    /// One extra source became ready. `index` is the offset in the vector
    /// `prepare_extra` filled for this pass.
    fn on_extra_ready(&mut self, _index: usize, _revents: i16) {}

    /// The composition root asked the loop to flush and return.
    fn should_stop(&self) -> bool {
        false
    }

    /// Final drain before the loop returns. No further tick runs.
    fn on_stop(&mut self) {}
}

#[cfg(unix)]
pub fn run<S, H>(listener: Option<TcpListener>, handler: &mut H, limits: &crate::limits::Limits)
where
    H: ServerLoop<State = S>,
{
    if let Some(listener) = &listener {
        let _ = listener.set_nonblocking(true);
    }
    let mut connections: Table<S> = HashMap::new();
    let mut next_id: u64 = 1;

    loop {
        if handler.should_stop() {
            handler.on_stop();
            return;
        }
        let now = Instant::now();
        let expired: Vec<u64> = connections
            .iter()
            .filter(|(_, connection)| handler.is_idle(connection, now, limits.idle_timeout))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(mut connection) = connections.remove(&id) {
                handler.on_close(&mut connection);
            }
        }

        handler.on_tick(&mut connections);

        let mut descriptors = Vec::with_capacity(connections.len() + 2);
        let mut keys = Vec::with_capacity(connections.len() + 2);
        descriptors.push(libc::pollfd {
            fd: listener
                .as_ref()
                .map(|listener| listener.as_raw_fd())
                .unwrap_or(-1),
            events: if listener.is_some() { libc::POLLIN } else { 0 },
            revents: 0,
        });
        keys.push(0);
        let wake = handler.wake_fd();
        descriptors.push(libc::pollfd {
            fd: wake.unwrap_or(-1),
            events: if wake.is_some() { libc::POLLIN } else { 0 },
            revents: 0,
        });
        keys.push(1);
        for (id, connection) in &connections {
            let mut events = handler.interest(connection);
            if connection.out_at < connection.out.len() {
                events |= libc::POLLOUT;
            }
            descriptors.push(libc::pollfd {
                fd: connection.stream.as_raw_fd(),
                events,
                revents: 0,
            });
            keys.push(*id);
        }

        let connection_end = descriptors.len();
        let mut extra_fds: Vec<(RawFd, i16)> = Vec::new();
        handler.prepare_extra(&mut extra_fds);
        let extra_start = descriptors.len();
        for (fd, events) in &extra_fds {
            descriptors.push(libc::pollfd {
                fd: *fd,
                events: *events,
                revents: 0,
            });
        }

        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                100,
            )
        };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                crate::log::svcd(&format!("svcd: server loop poll failed: {}", error));
            }
            continue;
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            // The handler drains the wake fd in on_tick; poll just returned early.
            handler.on_tick(&mut connections);
        }

        if descriptors[0].revents & libc::POLLIN != 0 {
            if let Some(listener) = &listener {
                loop {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let ip = peer.ip();
                            let per_ip = connections.values().filter(|c| c.peer == ip).count();
                            if connections.len() >= limits.max_connections
                                || per_ip >= limits.max_connections_per_ip
                            {
                                reject_connection(stream);
                                continue;
                            }
                            let _ = stream.set_nonblocking(true);
                            let id = next_id;
                            next_id = next_id.saturating_add(1);
                            let mut connection = Connection {
                                id,
                                stream,
                                peer: ip,
                                last_active: Instant::now(),
                                buffer: Vec::new(),
                                out: Vec::new(),
                                out_at: 0,
                                keep_alive: false,
                                head_only: false,
                                pending: None,
                                state: handler.new_state(id, ip),
                            };
                            handler.on_accept(&mut connection);
                            connections.insert(id, connection);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => {
                            crate::log::svcd(&format!(
                                "svcd: server loop accept failed: {}",
                                error
                            ));
                            break;
                        }
                    }
                }
            }
        }

        for index in 2..connection_end {
            let id = keys[index];
            let Some(connection) = connections.get_mut(&id) else {
                continue;
            };
            let revents = descriptors[index].revents;
            if revents & POLL_CLOSE != 0 {
                if let Some(mut connection) = connections.remove(&id) {
                    handler.on_poll_error(&mut connection, revents);
                    handler.on_close(&mut connection);
                }
                continue;
            }
            if revents & libc::POLLIN != 0 {
                if handler.reads_request(connection) {
                    let mut chunk = [0u8; 8 * 1024];
                    match connection.stream.read(&mut chunk) {
                        Ok(0) => {
                            if let Some(mut connection) = connections.remove(&id) {
                                handler.on_close(&mut connection);
                            }
                            continue;
                        }
                        Ok(read) => {
                            connection.last_active = Instant::now();
                            connection.buffer.extend_from_slice(&chunk[..read]);
                            match handler.pre_request(connection) {
                                PreRequest::Continue => {
                                    let id = connection.id;
                                    let peer = connection.peer;
                                    match advance_request(connection, limits.max_header, |head| {
                                        handler.route_request(id, peer, head)
                                    }) {
                                        Ok(None) => {}
                                        Ok(Some(request)) => {
                                            handler.on_request(connection, request);
                                        }
                                        Err(response) => {
                                            handler.on_error(connection, response);
                                        }
                                    }
                                }
                                PreRequest::Handled => {}
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => {
                            if let Some(mut connection) = connections.remove(&id) {
                                handler.on_close(&mut connection);
                            }
                            continue;
                        }
                    }
                } else if !handler.on_readable(connection) {
                    if let Some(mut connection) = connections.remove(&id) {
                        handler.on_close(&mut connection);
                    }
                    continue;
                }
            }
            let Some(connection) = connections.get_mut(&id) else {
                continue;
            };
            if revents & libc::POLLOUT != 0 || connection.out_at < connection.out.len() {
                if !handler.on_writable(connection) {
                    if let Some(mut connection) = connections.remove(&id) {
                        handler.on_close(&mut connection);
                    }
                }
            }
        }

        for offset in 0..extra_fds.len() {
            let revents = descriptors[extra_start + offset].revents;
            if revents != 0 {
                handler.on_extra_ready(offset, revents);
            }
        }
    }
}

#[cfg(unix)]
fn reject_connection(mut stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    let _ = std::io::Write::write_all(&mut stream, BUSY_CONNECTION_RESPONSE);
}

#[cfg(unix)]
pub fn advance_request<S>(
    connection: &mut Connection<S>,
    max_header: usize,
    mut route: impl FnMut(&http::Head) -> Result<BodyPolicy, Vec<u8>>,
) -> Result<Option<HttpRequest>, Vec<u8>> {
    if connection.pending.is_none() {
        // This request's method decides how its response is rendered. Clear
        // the previous request's flag first; an unparsable head leaves it
        // false because there is no method to trust.
        connection.head_only = false;
        let Some(header_end) = http::header_end(&connection.buffer) else {
            if connection.buffer.len() > max_header {
                return Err(crate::http::json_error(
                    400,
                    "bad_request",
                    "request headers are too large",
                ));
            }
            return Ok(None);
        };
        if header_end > max_header {
            return Err(crate::http::json_error(
                400,
                "bad_request",
                "request headers are too large",
            ));
        }
        let Some((head, body_start)) = http::parse_head(&connection.buffer[..header_end + 4])
        else {
            return Err(crate::http::json_error(
                400,
                "bad_request",
                "malformed request line",
            ));
        };
        connection.head_only = crate::http::is_head(&head.method);
        let framing = http::framing(&head.headers)
            .map_err(|message| crate::http::json_error(400, "bad_request", &message))?;
        // Route and authenticate before the body. While the body is still
        // arriving `pending` stays Some, so this callback runs exactly once per
        // request and the face never sees a different route for the same head.
        let body = match route(&head) {
            Ok(body) => body,
            Err(response) => {
                // A bodyless request can keep a refused connection alive: the
                // 401 is the whole exchange, and there is no unread body to
                // become the next request. A request that declared a body is
                // closed instead (the result is returned to on_error).
                if matches!(framing, http::Framing::None | http::Framing::Length(0)) {
                    let keep_alive = wants_keep_alive_head(&head);
                    let response = if keep_alive {
                        keep_alive_response(response)
                    } else {
                        response
                    };
                    connection.out = if connection.head_only {
                        http::strip_body(response)
                    } else {
                        response
                    };
                    connection.out_at = 0;
                    connection.keep_alive = keep_alive;
                    // Consume this head so the next poll does not route the
                    // same refused request again; any bytes after it are the
                    // next (unsupported) pipelined request and stay buffered.
                    let buffer = std::mem::take(&mut connection.buffer);
                    connection.buffer = buffer[body_start..].to_vec();
                    return Ok(None);
                }
                return Err(response);
            }
        };
        let limit = body.limit();
        if !body.accepts(framing) {
            return Err(crate::http::json_error(
                413,
                "payload_too_large",
                "request body is too large",
            ));
        }
        if body.confirms_body(&head, framing) {
            // Write the interim head in one non-blocking call. If it is only
            // partially accepted, close without appending a second status line:
            // a truncated 100 followed by a 400 would desync the client.
            match connection.stream.write(http::CONTINUE_RESPONSE) {
                Ok(written) if written == http::CONTINUE_RESPONSE.len() => {}
                _ => return Err(Vec::new()),
            }
        }
        let buffer = std::mem::take(&mut connection.buffer);
        connection.buffer = buffer[body_start..].to_vec();
        connection.pending = Some(PendingRequest {
            method: head.method,
            path: head.path,
            version: head.version,
            headers: head.headers,
            body: Vec::new(),
            framing: match framing {
                http::Framing::None | http::Framing::Length(0) => PendingFraming::None,
                http::Framing::Length(length) => PendingFraming::Length(length),
                http::Framing::Chunked => PendingFraming::Chunked(http::ChunkedDecoder::new(limit)),
            },
        });
    }

    let mut ready = false;
    if let Some(pending) = connection.pending.as_mut() {
        match &mut pending.framing {
            PendingFraming::None => ready = true,
            PendingFraming::Length(remaining) => {
                let take = std::cmp::min(*remaining, connection.buffer.len() as u64) as usize;
                if take > 0 {
                    pending.body.extend_from_slice(&connection.buffer[..take]);
                    connection.buffer.drain(..take);
                    *remaining -= take as u64;
                }
                ready = *remaining == 0;
            }
            PendingFraming::Chunked(decoder) => {
                let input = std::mem::take(&mut connection.buffer);
                decoder
                    .feed(&input, &mut |chunk| {
                        pending.body.extend_from_slice(chunk);
                        Ok(())
                    })
                    .map_err(|error| {
                        crate::http::json_error(
                            error.status(),
                            crate::http::code_for(error.status()),
                            error.detail(),
                        )
                    })?;
                ready = decoder.is_done();
            }
        }
    }
    if !ready {
        return Ok(None);
    }
    let pending = connection.pending.take().expect("pending request");
    Ok(Some(HttpRequest {
        method: pending.method,
        path: pending.path,
        version: pending.version,
        headers: pending.headers,
        body: pending.body,
    }))
}

/// HTTP/1.1 defaults to keep-alive unless the client opts out; HTTP/1.0 is the
/// opposite unless the client opts in. Pipelining is not supported: the next
/// request is read after the previous response is written.
pub fn wants_keep_alive(request: &HttpRequest) -> bool {
    wants_keep_alive_parts(&request.version, &request.headers)
}

/// Keep-alive for a head whose body has not been read yet. The Windows
/// fallback needs the decision before the body so a rejected request closes
/// instead of leaving unread bytes in front of the next request.
pub fn wants_keep_alive_head(head: &http::Head) -> bool {
    wants_keep_alive_parts(&head.version, &head.headers)
}

fn wants_keep_alive_parts(version: &str, headers: &[(String, String)]) -> bool {
    let connection = http::header(headers, "connection")
        .unwrap_or("")
        .to_ascii_lowercase();
    let close = connection.split(',').any(|token| token.trim() == "close");
    if close {
        return false;
    }
    if version.eq_ignore_ascii_case("HTTP/1.0") {
        return connection
            .split(',')
            .any(|token| token.trim() == "keep-alive");
    }
    true
}

pub fn keep_alive_response(mut response: Vec<u8>) -> Vec<u8> {
    const CLOSE: &[u8] = b"Connection: close\r\n";
    const KEEP: &[u8] = b"Connection: keep-alive\r\n";
    if let Some(position) = response
        .windows(CLOSE.len())
        .position(|window| window == CLOSE)
    {
        response.splice(position..position + CLOSE.len(), KEEP.iter().copied());
    }
    response
}

#[cfg(all(test, unix))]
mod tests {
    const M: usize = crate::limits::MAX_HEADER;

    use super::*;

    fn connection(buffer: &[u8]) -> Connection<()> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let client = TcpStream::connect(address).expect("client");
        let (stream, peer) = listener.accept().expect("accept");
        stream.set_nonblocking(true).expect("nonblocking");
        let mut connection = Connection {
            id: 1,
            stream,
            peer: peer.ip(),
            last_active: Instant::now(),
            buffer: Vec::new(),
            out: Vec::new(),
            out_at: 0,
            keep_alive: false,
            head_only: false,
            pending: None,
            state: (),
        };
        connection.buffer.extend_from_slice(buffer);
        drop(client);
        connection
    }

    #[test]
    fn the_route_callback_runs_once_while_a_chunked_body_arrives_in_pieces() {
        let mut connection =
            connection(b"POST /body HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n");
        let mut calls = 0;
        let mut route = |_head: &http::Head| {
            calls += 1;
            Ok(BodyPolicy::Buffered { limit: 64 })
        };

        assert!(advance_request(&mut connection, M, &mut route)
            .expect("head")
            .is_none());
        connection.buffer.extend_from_slice(b"4\r\npart\r\n");
        assert!(advance_request(&mut connection, M, &mut route)
            .expect("first chunk")
            .is_none());
        connection
            .buffer
            .extend_from_slice(b"4\r\nbody\r\n0\r\n\r\n");
        let request = advance_request(&mut connection, M, &mut route)
            .expect("last chunk")
            .expect("complete request");

        assert_eq!(request.body, b"partbody");
        assert_eq!(
            calls, 1,
            "route must be resolved once, before the body is read"
        );
    }

    #[test]
    fn an_empty_route_refuses_a_declared_body_before_any_body_byte_is_read() {
        let mut connection =
            connection(b"POST /action HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n");
        let response = advance_request(&mut connection, M, |_head| Ok(BodyPolicy::Empty))
            .expect_err("an empty route must refuse a declared body");
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 413"),
            "{}",
            String::from_utf8_lossy(&response)
        );
    }
}
