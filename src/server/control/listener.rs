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
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use crate::channel::{PendingCall, Reply};
use crate::ipc;
use crate::rpc;
use crate::wake::Wake;

/// One poll source owned by the control server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Listener,
    Connection(u64),
}

struct Connection {
    stream: UnixStream,
    buffer: Vec<u8>,
    out: Vec<u8>,
    out_at: usize,
    pending: Option<Receiver<String>>,
    close_after_write: bool,
}

pub struct ControlServer {
    listener: UnixListener,
    connections: HashMap<u64, Connection>,
    next_id: u64,
    tx: Sender<PendingCall>,
    wake: Arc<Wake>,
    /// Shared with `Reply::with_notify`: the supervisor writes one byte after
    /// it queues a reply so the server loop can write it immediately.
    notify: Arc<UnixStream>,
}

impl ControlServer {
    pub fn new(
        listener: UnixListener,
        tx: Sender<PendingCall>,
        wake: Arc<Wake>,
        notify: Arc<UnixStream>,
    ) -> io::Result<ControlServer> {
        listener.set_nonblocking(true)?;
        Ok(ControlServer {
            listener,
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
        out.push((self.listener.as_raw_fd(), libc::POLLIN));
        keys.push(Source::Listener);
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
            Source::Listener => self.accept(),
            Source::Connection(id) => self.ready_connection(id, revents),
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
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
                        },
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    crate::log::svcd(&format!("svcd: control accept failed: {}", error));
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
        for connection in self.connections.values_mut() {
            let Some(receiver) = &connection.pending else {
                continue;
            };
            let mut frames = Vec::new();
            while let Ok(frame) = receiver.try_recv() {
                frames.push(frame);
            }
            for frame in frames {
                let final_frame = serde_json::from_str::<serde_json::Value>(&frame)
                    .ok()
                    .and_then(|value| value.get("id").cloned())
                    .is_some();
                connection.out.extend_from_slice(frame.as_bytes());
                connection.out.push(b'\n');
                if final_frame {
                    connection.pending = None;
                }
            }
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
    tx: &Sender<PendingCall>,
    wake: &Arc<Wake>,
    notify: Arc<UnixStream>,
) -> bool {
    loop {
        let Some(position) = connection.buffer.iter().position(|byte| *byte == b'\n') else {
            if connection.buffer.len() > ipc::LOCAL_FRAME_MAX {
                connection.out = rpc::to_line(&rpc::err(
                    None,
                    -32700,
                    "management frame exceeds the local limit",
                ))
                .into_bytes();
                connection.out.push(b'\n');
                connection.close_after_write = true;
            }
            return true;
        };
        if position > ipc::LOCAL_FRAME_MAX {
            connection.out = rpc::to_line(&rpc::err(
                None,
                -32700,
                "management frame exceeds the local limit",
            ))
            .into_bytes();
            connection.out.push(b'\n');
            connection.close_after_write = true;
            return true;
        }
        let line: Vec<u8> = connection.buffer.drain(..=position).collect();
        let line = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).to_string();
        let (sender, receiver) = mpsc::channel();
        if tx
            .send(PendingCall {
                line,
                reply: Reply::with_notify(sender, notify),
            })
            .is_err()
        {
            return false;
        }
        wake.notify();
        connection.pending = Some(receiver);
        return true;
    }
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
    use std::io::{BufRead, BufReader, Write};
    use std::time::Instant;

    /// Two control connections send different request ids. The reply must go
    /// to the connection that asked, never to whichever socket is first.
    #[test]
    fn replies_go_back_to_the_connection_that_sent_the_request() {
        let root = std::env::temp_dir().join(format!(
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
        let (notify_tx, _notify_rx) = UnixStream::pair().expect("notify");
        let notify_tx = Arc::new(notify_tx);
        let (tx, rx) = mpsc::channel();
        let mut server =
            ControlServer::new(listener, tx, Arc::new(Wake::new()), notify_tx).expect("server");
        let supervisor = std::thread::spawn(move || {
            for call in rx {
                let value: serde_json::Value = serde_json::from_str(&call.line).expect("request");
                let id = value.get("id").cloned();
                let reply = rpc::to_line(&rpc::ok(id, serde_json::json!({ "ok": true })));
                let _ = call.reply.send(reply);
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
}
