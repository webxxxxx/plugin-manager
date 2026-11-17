//! The thread-per-connection driver: one thread per accepted socket, used by
//! hosts whose server has no poll loop.
//!
//! Moved out of `api.rs` (#139) so the composition root keeps no platform
//! branch: sockets, admission and framing live here, and the routing/faces are
//! injected as a closure. Everything compiles on every host; the only platform
//! difference is the disconnect probe (WinSock `select`, #135), which stays in
//! this file.

#![allow(dead_code)] // unused on hosts with a poll loop

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::http;
use crate::lan::Lan;
use crate::log;
use crate::server::http::engine;
use crate::threads;

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

/// One accepted connection, handed to the composition root's handler.
pub(crate) type Connection =
    Arc<dyn Fn(TcpStream, IpAddr, Arc<Config>, Arc<Mutex<Lan>>, Duration) + Send + Sync>;

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
/// socket and the per-connection settings; the composition root supplies it so
/// this module never names a face.
#[allow(clippy::too_many_arguments)]
pub(crate) fn serve(
    active: TcpListener,
    config: Arc<Config>,
    lan: Arc<Mutex<Lan>>,
    admission: Arc<Mutex<Admission>>,
    idle_timeout: Duration,
    stop: Arc<AtomicBool>,
    connection: Connection,
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
                    .stack_size(threads::transfer_stack(&limits))
                    .spawn(move || {
                        let _lease = lease;
                        connection(stream, ip, config, lan, idle_timeout);
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
/// Write one rendered response. HEAD keeps the head (including GET's
/// `Content-Length`) and gets no body, so a keep-alive connection cannot read
/// the omitted body as the start of the next response.
pub(crate) fn write_http(
    stream: &mut TcpStream,
    response: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    if head_only {
        stream.write_all(&http::strip_body(response.to_vec()))
    } else {
        stream.write_all(response)
    }
}

/// Drive a face-produced stream on the Windows connection thread. The
/// fallback owns the socket and the shared max_streams slot; a Busy result is
/// rendered before any handler bytes are written.
pub(crate) fn drive_fallback(
    stream: &mut TcpStream,
    handler: Box<dyn crate::server::http::StreamHandler>,
    head_only: bool,
    limits: &crate::limits::Limits,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(limits.transfer_timeout));
    if crate::server::http::fallback::drive(stream, handler, &[], limits).is_err() {
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

/// Whether the SSE client has gone away (#135).
///
/// `peek` on a blocking socket waits for data, so a readiness check with a
/// zero timeout must come first; only a readable socket is peeked. A graceful
/// FIN is `Ok(0)`, and an error means the peer reset the connection: both mean
/// the SSE response can no longer be delivered. This is the Windows equivalent
/// of the Unix stream driver's cancel token, not a permanent exemption.
pub(crate) fn client_gone(stream: &TcpStream) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            select, FD_SET, SOCKET, SOCKET_ERROR, TIMEVAL,
        };

        let mut readable: FD_SET = unsafe { std::mem::zeroed() };
        readable.fd_count = 1;
        readable.fd_array[0] = stream.as_raw_socket() as SOCKET;
        let timeout = TIMEVAL {
            tv_sec: 0,
            tv_usec: 0,
        };
        let ready = unsafe {
            select(
                0,
                &mut readable,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &timeout,
            )
        };
        if ready == SOCKET_ERROR {
            // A transient winsock failure is not proof that the client left;
            // cancelling on it would truncate a live SSE body. A real reset still
            // surfaces as readable + peek error, and a write failure flips the
            // caller's abandonment flag.
            return false;
        }
        if ready == 0 {
            return false;
        }
        // Readable now, so this peek cannot block. A FIN shows up as `Ok(0)`.
        let mut byte = [0u8; 1];
        match stream.peek(&mut byte) {
            Ok(0) => true,
            Ok(_) => {
                // Pipelined or stray bytes on a `Connection: close` response can
                // never be served; drain them so a FIN queued behind them becomes
                // visible instead of leaving the socket readable forever.
                let mut junk = [0u8; 64];
                let mut socket = stream;
                let _ = socket.read(&mut junk);
                false
            }
            Err(_) => true,
        }
    }
    #[cfg(not(windows))]
    {
        // The Unix server loop owns disconnect detection; this body only
        // exists so the shared driver compiles. A FIN shows up as a
        // zero-length peek.
        let mut byte = [0u8; 1];
        matches!(stream.peek(&mut byte), Ok(0) | Err(_))
    }
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
