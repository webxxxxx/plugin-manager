//! The local control channel, owned by the server loop.
//!
//! One poll set serves every CLI/HTTP/MCP control connection. Requests go to
//! the supervisor loop through its bounded `PendingCall` channel; replies come
//! back through `Reply` and the shared wake pipe, and this module writes them
//! to the owning control connection. The supervisor never touches a socket.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use crate::kernel::failure::Problem;
use crate::platform::host::wake::Wake;
use crate::wire::control::channel::{Endpoint, Notify, PendingCall, Reply};
use crate::wire::message::ipc;
use crate::wire::message::rpc;

/// One poll source owned by the control server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Listener(Endpoint),
    Connection(u64),
}

struct Connection {
    stream: UnixStream,
    buffer: Vec<u8>,
    out: Vec<u8>,
    out_at: usize,
    pending: Option<Receiver<String>>,
    close_after_write: bool,
    /// The endpoint this connection was accepted on; every request it carries
    /// inherits the tier.
    endpoint: Endpoint,
}

pub struct ControlServer {
    user: UnixListener,
    admin: UnixListener,
    connections: HashMap<u64, Connection>,
    next_id: u64,
    tx: SyncSender<PendingCall>,
    wake: Arc<Wake>,
    /// Shared with `Reply::with_notify`: the supervisor writes one byte after
    /// it queues a reply so the server loop can write it immediately.
    notify: Arc<UnixStream>,
}

impl ControlServer {
    pub fn new(
        user: UnixListener,
        admin: UnixListener,
        tx: SyncSender<PendingCall>,
        wake: Arc<Wake>,
        notify: Arc<UnixStream>,
    ) -> io::Result<ControlServer> {
        user.set_nonblocking(true)?;
        admin.set_nonblocking(true)?;
        Ok(ControlServer {
            user,
            admin,
            connections: HashMap::new(),
            next_id: 1,
            tx,
            wake,
            notify,
        })
    }

    /// Append this server's poll entries. The offset in `out` is the index
    /// passed to `ready`; `keys` carries the stable source for that index.
    pub fn poll_sources(&self, out: &mut Vec<(RawFd, i16)>, keys: &mut Vec<Source>) {
        out.push((self.user.as_raw_fd(), libc::POLLIN));
        keys.push(Source::Listener(Endpoint::User));
        out.push((self.admin.as_raw_fd(), libc::POLLIN));
        keys.push(Source::Listener(Endpoint::Admin));
        for (id, connection) in &self.connections {
            let mut events = 0;
            if connection.pending.is_none() && connection.out.is_empty() {
                events |= libc::POLLIN;
            }
            if !connection.out.is_empty() {
                events |= libc::POLLOUT;
            }
            // A waiting connection still enters poll with zero events so a
            // POLLHUP from a client that gave up is observed; otherwise the
            // reply slot would leak until the supervisor answers.
            out.push((connection.stream.as_raw_fd(), events));
            keys.push(Source::Connection(*id));
        }
    }

    pub fn ready(&mut self, source: Source, revents: i16) {
        match source {
            Source::Listener(endpoint) => self.accept(endpoint),
            Source::Connection(id) => self.ready_connection(id, revents),
        }
    }

    fn accept(&mut self, endpoint: Endpoint) {
        let listener = match endpoint {
            Endpoint::User => &self.user,
            Endpoint::Admin => &self.admin,
        };
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    let id = self.next_id;
                    self.next_id = self.next_id.saturating_add(1);
                    self.connections.insert(
                        id,
                        Connection {
                            stream,
                            buffer: Vec::new(),
                            out: Vec::new(),
                            out_at: 0,
                            pending: None,
                            close_after_write: false,
                            endpoint,
                        },
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    crate::platform::log::svcd(&format!("svcd: control accept failed: {}", error));
                    break;
                }
            }
        }
    }

    fn ready_connection(&mut self, id: u64, revents: i16) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        if revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            self.connections.remove(&id);
            return;
        }
        if revents & libc::POLLIN != 0 && connection.pending.is_none() && connection.out.is_empty()
        {
            let mut chunk = [0u8; 8 * 1024];
            match connection.stream.read(&mut chunk) {
                Ok(0) => {
                    self.connections.remove(&id);
                    return;
                }
                Ok(read) => {
                    connection.buffer.extend_from_slice(&chunk[..read]);
                    let keep =
                        drain_requests(connection, &self.tx, &self.wake, self.notify.clone());
                    if !keep {
                        self.connections.remove(&id);
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    self.connections.remove(&id);
                    return;
                }
            }
        }
        if revents & libc::POLLOUT != 0 {
            write_connection(&mut self.connections, id);
        }
    }

    /// Move every queued supervisor reply onto its control connection's output
    /// buffer. Called from the server loop tick (the shared wake pipe fired, or
    /// the 100 ms poll expired).
    pub fn drain_replies(&mut self) {
        let mut dead = Vec::new();
        for (id, connection) in self.connections.iter_mut() {
            let Some(receiver) = connection.pending.take() else {
                continue;
            };
            let mut frames = Vec::new();
            let mut disconnected = false;
            loop {
                match receiver.try_recv() {
                    Ok(frame) => frames.push(frame),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // The supervisor dropped the reply without a final
                        // frame (shutdown or a crashed loop). Close the client
                        // instead of leaving it waiting forever.
                        disconnected = true;
                        break;
                    }
                }
            }
            let mut final_frame_seen = false;
            for frame in frames {
                let final_frame = serde_json::from_str::<serde_json::Value>(&frame)
                    .ok()
                    .and_then(|value| value.get("id").cloned())
                    .is_some();
                connection.out.extend_from_slice(frame.as_bytes());
                connection.out.push(b'\n');
                if final_frame {
                    final_frame_seen = true;
                }
            }
            if final_frame_seen {
                // Keep the connection for the next request; the reply is
                // queued in the output buffer and `flush` writes it. A normal
                // per-call sender drop after the final frame must not close a
                // reusable CLI connection.
            } else if disconnected {
                // No final frame and no sender: the supervisor is gone. Deliver
                // whatever progress frames were already collected, then close.
                if connection.out.is_empty() {
                    dead.push(*id);
                } else {
                    connection.close_after_write = true;
                }
            } else {
                connection.pending = Some(receiver);
            }
        }
        for id in dead {
            self.connections.remove(&id);
        }
    }

    /// Write whatever is queued. The engine also calls this on POLLOUT; doing
    /// it after a reply batch avoids a second poll round trip.
    pub fn flush(&mut self) {
        let ids: Vec<u64> = self.connections.keys().copied().collect();
        for id in ids {
            write_connection(&mut self.connections, id);
        }
    }

    /// Shutdown drain: take the queued replies and flush every control socket
    /// with a short blocking timeout. Called once, after the supervisor loop
    /// has returned (so no reply can arrive later).
    pub fn drain_for_shutdown(&mut self) {
        self.drain_replies();
        for (_, connection) in std::mem::take(&mut self.connections) {
            if connection.out_at >= connection.out.len() {
                continue;
            }
            let _ = connection.stream.set_nonblocking(false);
            let _ = connection
                .stream
                .set_write_timeout(Some(Duration::from_millis(500)));
            let mut stream = &connection.stream;
            let _ = stream.write_all(&connection.out[connection.out_at..]);
        }
    }
}

fn drain_requests(
    connection: &mut Connection,
    tx: &SyncSender<PendingCall>,
    wake: &Arc<Wake>,
    notify: Arc<UnixStream>,
) -> bool {
    let Some(position) = connection.buffer.iter().position(|byte| *byte == b'\n') else {
        if connection.buffer.len() > ipc::LOCAL_FRAME_MAX {
            connection.out = rpc::to_line(&rpc::err_data(
                None,
                -32700,
                "management frame exceeds the local limit",
                Problem::Invalid.error_data(),
            ))
            .into_bytes();
            connection.out.push(b'\n');
            connection.close_after_write = true;
        }
        return true;
    };
    if position > ipc::LOCAL_FRAME_MAX {
        connection.out = rpc::to_line(&rpc::err_data(
            None,
            -32700,
            "management frame exceeds the local limit",
            Problem::Invalid.error_data(),
        ))
        .into_bytes();
        connection.out.push(b'\n');
        connection.close_after_write = true;
        return true;
    }
    let line: Vec<u8> = connection.buffer.drain(..=position).collect();
    let line = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).to_string();
    let (sender, receiver) = mpsc::channel();
    let reply = Reply::with_notify(sender, Notify::new(notify));
    match tx.try_send(PendingCall {
        line,
        reply,
        endpoint: connection.endpoint,
    }) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            // Bounded backpressure: answer this connection with a visible
            // error and close it instead of growing the supervisor queue.
            connection.out = rpc::to_line(&rpc::err_data(
                None,
                -32000,
                "control queue is full; retry later",
                Problem::Internal.error_data(),
            ))
            .into_bytes();
            connection.out.push(b'\n');
            connection.close_after_write = true;
            return true;
        }
        Err(TrySendError::Disconnected(_)) => return false,
    }
    wake.notify();
    connection.pending = Some(receiver);
    true
}

fn write_connection(connections: &mut HashMap<u64, Connection>, id: u64) {
    let Some(connection) = connections.get_mut(&id) else {
        return;
    };
    if connection.out_at >= connection.out.len() {
        return;
    }
    match connection
        .stream
        .write(&connection.out[connection.out_at..])
    {
        Ok(0) => {
            connections.remove(&id);
        }
        Ok(written) => {
            connection.out_at += written;
            if connection.out_at >= connection.out.len() {
                connection.out.clear();
                connection.out_at = 0;
                if connection.close_after_write {
                    connections.remove(&id);
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(_) => {
            connections.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::Instant;

    /// Two control connections send different request ids. The reply must go
    /// to the connection that asked, never to whichever socket is first.
    #[test]
    fn replies_go_back_to_the_connection_that_sent_the_request() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-control-route-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("svcd.sock");
        let listener = UnixListener::bind(&path).expect("listener");
        let admin = UnixListener::bind(root.join("svcd-admin.sock")).expect("admin listener");
        let (notify_tx, _notify_rx) = UnixStream::pair().expect("notify");
        let notify_tx = Arc::new(notify_tx);
        let (tx, rx) = mpsc::sync_channel::<PendingCall>(8);
        let mut server = ControlServer::new(listener, admin, tx, Arc::new(Wake::new()), notify_tx)
            .expect("server");
        let supervisor = std::thread::spawn(move || {
            for call in rx {
                let value: serde_json::Value = serde_json::from_str(&call.line).expect("request");
                let id = value.get("id").cloned();
                let reply = rpc::to_line(&rpc::ok(id, serde_json::json!({ "ok": true })));
                call.reply.send(reply);
            }
        });

        let (done_tx, done_rx) = mpsc::channel();
        for id in [1u64, 2] {
            let path = path.clone();
            let done = done_tx.clone();
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&path).expect("connect");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("timeout");
                let request = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"svcd.ping\"}}\n",
                    id
                );
                stream.write_all(request.as_bytes()).expect("write");
                let mut line = String::new();
                BufReader::new(stream)
                    .read_line(&mut line)
                    .expect("reply line");
                let _ = done.send((id, line));
            });
        }
        drop(done_tx);

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut replies = Vec::new();
        while replies.len() < 2 && Instant::now() < deadline {
            let mut fds = Vec::new();
            let mut keys = Vec::new();
            server.poll_sources(&mut fds, &mut keys);
            let mut descriptors: Vec<libc::pollfd> = fds
                .iter()
                .map(|(fd, events)| libc::pollfd {
                    fd: *fd,
                    events: *events,
                    revents: 0,
                })
                .collect();
            // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
            unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    50,
                )
            };
            for (index, key) in keys.iter().enumerate() {
                if descriptors[index].revents != 0 {
                    server.ready(*key, descriptors[index].revents);
                }
            }
            server.drain_replies();
            server.flush();
            while let Ok(reply) = done_rx.try_recv() {
                replies.push(reply);
            }
        }

        assert_eq!(replies.len(), 2, "both control connections got a reply");
        for (sent_id, line) in replies {
            let value: serde_json::Value = serde_json::from_str(line.trim()).expect("reply");
            assert_eq!(value["id"], sent_id, "{line}");
            assert!(value.get("error").is_none(), "{line}");
        }
        drop(server);
        let _ = supervisor.join();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The shutdown path must flush a reply that was queued but never reached
    /// a normal writable/tick pass. This is exactly the last shutdown response.
    #[test]
    fn shutdown_drains_a_reply_that_was_queued_but_not_flushed() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-control-drain-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("svcd.sock");
        let listener = UnixListener::bind(&path).expect("listener");
        let admin = UnixListener::bind(root.join("svcd-admin.sock")).expect("admin listener");
        let (notify_tx, _notify_rx) = UnixStream::pair().expect("notify");
        let notify_tx = Arc::new(notify_tx);
        let (tx, rx) = mpsc::sync_channel::<PendingCall>(8);
        let mut server = ControlServer::new(listener, admin, tx, Arc::new(Wake::new()), notify_tx)
            .expect("server");
        let (queued_tx, queued_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for call in rx {
                let value: serde_json::Value = serde_json::from_str(&call.line).expect("request");
                let id = value.get("id").cloned();
                call.reply.send(rpc::to_line(&rpc::ok(
                    id,
                    serde_json::json!({ "ok": true }),
                )));
                let _ = queued_tx.send(());
            }
        });

        let client = {
            let path = path.clone();
            std::thread::spawn(move || {
                let mut stream = UnixStream::connect(&path).expect("connect");
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .expect("timeout");
                stream
                    .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"svcd.shutdown\"}\n")
                    .expect("write");
                let mut line = String::new();
                BufReader::new(stream)
                    .read_line(&mut line)
                    .expect("drained reply");
                line
            })
        };

        // Accept and read the request, but deliberately never call
        // drain_replies/flush: the reply stays in the pending receiver.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut queued = false;
        while !queued && Instant::now() < deadline {
            if queued_rx.try_recv().is_ok() {
                queued = true;
                break;
            }
            let mut fds = Vec::new();
            let mut keys = Vec::new();
            server.poll_sources(&mut fds, &mut keys);
            let mut descriptors: Vec<libc::pollfd> = fds
                .iter()
                .map(|(fd, events)| libc::pollfd {
                    fd: *fd,
                    events: *events,
                    revents: 0,
                })
                .collect();
            // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
            unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    50,
                )
            };
            for (index, key) in keys.iter().enumerate() {
                if descriptors[index].revents != 0 {
                    server.ready(*key, descriptors[index].revents);
                }
            }
        }
        assert!(queued, "the supervisor queued the reply");
        server.drain_for_shutdown();
        let line = client.join().expect("client");
        let value: serde_json::Value = serde_json::from_str(line.trim()).expect("reply");
        assert_eq!(value["id"], 9, "{line}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A supervisor that disappears after a progress frame but before the final
    /// response must not drop the collected frame: the loop writes it, then
    /// closes instead of leaving the client waiting forever.
    #[test]
    fn a_sender_that_disappears_without_a_final_frame_flushes_then_closes() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-control-progress-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("svcd.sock");
        let listener = UnixListener::bind(&path).expect("listener");
        let admin = UnixListener::bind(root.join("svcd-admin.sock")).expect("admin listener");
        let (notify_tx, _notify_rx) = UnixStream::pair().expect("notify");
        let (tx, rx) = mpsc::sync_channel::<PendingCall>(8);
        let mut server = ControlServer::new(
            listener,
            admin,
            tx,
            Arc::new(Wake::new()),
            Arc::new(notify_tx),
        )
        .expect("server");

        let mut client = UnixStream::connect(&path).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        server.ready(Source::Listener(Endpoint::User), libc::POLLIN);
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"registry.call\"}\n")
            .expect("write");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut call = None;
        while call.is_none() && Instant::now() < deadline {
            let mut fds = Vec::new();
            let mut keys = Vec::new();
            server.poll_sources(&mut fds, &mut keys);
            for source in keys {
                if let Source::Connection(id) = source {
                    server.ready(Source::Connection(id), libc::POLLIN);
                }
            }
            if let Ok(next) = rx.try_recv() {
                call = Some(next);
            }
        }
        let call = call.expect("request reached the supervisor");
        call.reply
            .send("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}".to_string());
        drop(call);

        server.drain_replies();
        assert!(
            server
                .connections
                .values()
                .any(|connection| connection.close_after_write),
            "the collected progress frame is queued and the connection closes after it"
        );
        server.flush();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).expect("progress frame");
        assert!(line.contains("notifications/progress"), "{line}");
        let mut eof = [0u8; 1];
        assert_eq!(
            reader.read(&mut eof).expect("eof after progress"),
            0,
            "the connection closes after delivering the collected frame"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A full supervisor queue must answer this connection with a visible
    /// error and close it, never grow the queue.
    #[test]
    fn a_full_control_queue_answers_an_error_instead_of_growing() {
        let (stream, _peer) = UnixStream::pair().expect("pair");
        let (tx, _rx) = mpsc::sync_channel(0);
        let (_notify_tx, notify_rx) = UnixStream::pair().expect("notify");
        let notify = Arc::new(notify_rx);
        let mut connection = Connection {
            stream,
            buffer: b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"svcd.ping\"}\n".to_vec(),
            out: Vec::new(),
            out_at: 0,
            pending: None,
            close_after_write: false,
            endpoint: Endpoint::User,
        };
        assert!(drain_requests(
            &mut connection,
            &tx,
            &Arc::new(Wake::new()),
            notify
        ));
        assert!(connection.close_after_write);
        let text = String::from_utf8_lossy(&connection.out);
        assert!(text.contains("control queue is full"), "{text}");
        assert!(text.ends_with('\n'), "{text}");
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("error frame");
        assert_eq!(value["error"]["code"], -32000);
        assert_eq!(value["error"]["data"]["kind"], "internal");
    }

    /// The depth is derived now (`pending_queue`), so the queue the daemon
    /// builds from a derived limit must still answer busy when it is full
    /// instead of waiting for room.
    #[test]
    fn a_derived_one_slot_queue_answers_busy_on_the_second_request() {
        let mut limits = crate::platform::limits::test_default();
        limits.pending_queue = 1;
        let (tx, rx) = mpsc::sync_channel(limits.pending_queue);
        let notify = Arc::new(UnixStream::pair().expect("notify").1);
        let wake = Arc::new(Wake::new());

        let connection = |_tx: &mpsc::SyncSender<PendingCall>| {
            let (stream, _peer) = UnixStream::pair().expect("pair");
            Connection {
                stream,
                buffer: b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"svcd.ping\"}\n".to_vec(),
                out: Vec::new(),
                out_at: 0,
                pending: None,
                close_after_write: false,
                endpoint: Endpoint::User,
            }
        };

        let mut first = connection(&tx);
        assert!(drain_requests(&mut first, &tx, &wake, notify.clone()));
        assert!(!first.close_after_write, "one slot is not full yet");

        let mut second = connection(&tx);
        assert!(drain_requests(&mut second, &tx, &wake, notify));
        assert!(second.close_after_write);
        let text = String::from_utf8_lossy(&second.out);
        assert!(text.contains("control queue is full"), "{text}");
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("error frame");
        assert_eq!(value["error"]["code"], -32000);

        // The first request was still queued; the second never grew the queue.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err(), "the queue did not grow");
    }

    /// An oversized control frame is a JSON-RPC parse error and carries the
    /// `invalid` kind, so the face never has to parse the message.
    #[test]
    fn an_oversized_control_frame_carries_the_invalid_kind() {
        let (stream, _peer) = UnixStream::pair().expect("pair");
        let (tx, _rx) = mpsc::sync_channel(0);
        let (_notify_tx, notify_rx) = UnixStream::pair().expect("notify");
        let mut connection = Connection {
            stream,
            buffer: vec![b'x'; ipc::LOCAL_FRAME_MAX + 1],
            out: Vec::new(),
            out_at: 0,
            pending: None,
            close_after_write: false,
            endpoint: Endpoint::Admin,
        };
        let keep = drain_requests(
            &mut connection,
            &tx,
            &Arc::new(Wake::new()),
            Arc::new(notify_rx),
        );
        assert!(keep);
        assert!(connection.close_after_write);
        let text = String::from_utf8_lossy(&connection.out);
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("error frame");
        assert_eq!(value["error"]["code"], -32700);
        assert_eq!(value["error"]["data"]["kind"], "invalid");
    }
}
