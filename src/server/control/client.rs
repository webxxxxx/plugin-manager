//! Outbound management streaming client, owned by the server loop.
//!
//! A streaming `registry.call` (MCP progress) opens one management-channel
//! connection. The server loop multiplexes those sockets with everything else;
//! progress lines go into the owning MCP stream handler's bounded queue, and a
//! full queue stops reading the upstream socket instead of growing memory.
//!
//! The request write is nonblocking: a large `tools/call` frame can be larger
//! than the socket buffer, and the control listener is polled by this same
//! loop, so blocking here would deadlock the daemon. A connection whose
//! channel cannot be opened immediately still returns a stream id and sends
//! `ProgressEvent::Error`, preserving the old 200 + -32603 SSE response
//! instead of turning it into an HTTP 500 before the stream starts.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::channel::{self, ProgressEvent};
use crate::ipc;

struct Connection {
    stream: UnixStream,
    buffer: Vec<u8>,
    /// The request frame while it is still being written. A nonblocking local
    /// socket can take only part of a large frame per pass.
    request: Vec<u8>,
    request_at: usize,
    sender: SyncSender<ProgressEvent>,
    blocked: bool,
    pending: Option<ProgressEvent>,
}

/// Cancel queue shared with a stream handler. The handler drops on the server
/// loop (inside a connection close or a response), so it cannot borrow the
/// client; it queues the id and the loop removes the stream on its next tick.
#[derive(Clone, Default)]
pub struct StreamCancel {
    ids: Arc<Mutex<Vec<u64>>>,
}

impl StreamCancel {
    pub fn cancel(&self, id: u64) {
        self.ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(id);
    }
}

pub struct StreamingClient {
    connections: BTreeMap<u64, Connection>,
    next_id: u64,
    cancels: StreamCancel,
}

impl StreamingClient {
    pub fn new() -> StreamingClient {
        StreamingClient {
            connections: BTreeMap::new(),
            next_id: 1,
            cancels: StreamCancel::default(),
        }
    }

    pub fn canceller(&self) -> StreamCancel {
        self.cancels.clone()
    }

    /// Open one streaming management request and return its stream id. The
    /// connection's request frame is written without blocking; its progress
    /// lines are delivered to `sender`. Every local failure is delivered as
    /// `ProgressEvent::Error`, so the MCP stream keeps its 200 + -32603 shape
    /// instead of turning into an HTTP 500 before the stream starts.
    pub fn open(
        &mut self,
        config: &Path,
        method: &str,
        params: Value,
        sender: SyncSender<ProgressEvent>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = match management_request(config, method, params) {
            Ok(request) => request,
            Err(message) => {
                let _ = sender.try_send(ProgressEvent::Error(message));
                return id;
            }
        };
        // A local AF_UNIX connect completes from the listener's backlog; the
        // accept happens later in this same loop. Pending outbound connects are
        // bounded by `max_streams` (at most 64), below the listener's 128-entry
        // backlog, so this inline connect does not wait for accept and cannot
        // fill the backlog the loop must drain.
        let stream = match UnixStream::connect(config.join("svcd.sock")) {
            Ok(stream) => stream,
            Err(error) => {
                let _ =
                    sender.try_send(ProgressEvent::Error(format!("svcd unreachable: {}", error)));
                return id;
            }
        };
        if let Err(error) = stream.set_nonblocking(true) {
            let _ = sender.try_send(ProgressEvent::Error(format!("svcd unreachable: {}", error)));
            return id;
        }
        let mut connection = Connection {
            stream,
            buffer: Vec::new(),
            request,
            request_at: 0,
            sender,
            blocked: false,
            pending: None,
        };
        if !write_request(&mut connection) {
            let _ = connection.sender.try_send(ProgressEvent::Error(
                "cannot write management request".into(),
            ));
            return id;
        }
        self.connections.insert(id, connection);
        id
    }

    /// Remove streams whose MCP handler already dropped or finished.
    pub fn drain_cancels(&mut self) {
        let ids: Vec<u64> = self
            .cancels
            .ids
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drain(..)
            .collect();
        for id in ids {
            self.connections.remove(&id);
        }
    }

    /// Append this client's poll entries, using the same index space as
    /// `ready`. A stream whose request is still being written waits for
    /// POLLOUT; a blocked stream is still polled with zero events so a peer
    /// close is observed, but its upstream is not read.
    pub fn poll_sources(&self, out: &mut Vec<(RawFd, i16)>, keys: &mut Vec<u64>) {
        for (id, connection) in &self.connections {
            let events = if connection.request_at < connection.request.len() {
                libc::POLLOUT
            } else if connection.blocked {
                0
            } else {
                libc::POLLIN
            };
            out.push((connection.stream.as_raw_fd(), events));
            keys.push(*id);
        }
    }

    pub fn ready(&mut self, id: u64, revents: i16) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
        if revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            self.connections.remove(&id);
            return;
        }
        if connection.request_at < connection.request.len() {
            if revents & libc::POLLOUT == 0 {
                return;
            }
            if !write_request(connection) {
                let _ = connection.sender.try_send(ProgressEvent::Error(
                    "cannot write management request".into(),
                ));
                self.connections.remove(&id);
                return;
            }
            if connection.request_at < connection.request.len() {
                return;
            }
            connection.request.clear();
            return;
        }
        if revents & libc::POLLIN != 0 {
            let mut chunk = [0u8; 8 * 1024];
            match connection.stream.read(&mut chunk) {
                Ok(0) => {
                    self.connections.remove(&id);
                    return;
                }
                Ok(read) => {
                    connection.buffer.extend_from_slice(&chunk[..read]);
                    if !drain_connection(connection) {
                        self.connections.remove(&id);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    self.connections.remove(&id);
                }
            }
        }
    }

    /// Retry one event that did not fit the handler queue. Called every server
    /// loop pass; a 100 ms delay is the same cadence the old reactor used.
    pub fn tick(&mut self) {
        let blocked: Vec<u64> = self
            .connections
            .iter()
            .filter(|(_, connection)| connection.blocked)
            .map(|(id, _)| *id)
            .collect();
        for id in blocked {
            let Some(connection) = self.connections.get_mut(&id) else {
                continue;
            };
            connection.blocked = false;
            if !drain_connection(connection) {
                self.connections.remove(&id);
            }
        }
    }

    /// Drop every outbound stream. Dropping the sender makes its MCP handler
    /// see a disconnected channel, which maps to the legacy -32603.
    pub fn close(&mut self) {
        self.connections.clear();
    }
}

fn management_request(config: &Path, method: &str, params: Value) -> Result<Vec<u8>, String> {
    let token =
        channel::read_token(config).map_err(|error| format!("svcd unreachable: {}", error))?;
    let line = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
        "auth": token,
    })
    .to_string();
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');
    if bytes.len() > ipc::LOCAL_FRAME_MAX {
        return Err("management request exceeds the local limit".into());
    }
    Ok(bytes)
}

/// Write as much of the queued request as the socket takes. Returns false on a
/// real write error; a short write leaves the rest for POLLOUT.
fn write_request(connection: &mut Connection) -> bool {
    while connection.request_at < connection.request.len() {
        match connection
            .stream
            .write(&connection.request[connection.request_at..])
        {
            Ok(0) => return false,
            Ok(written) => connection.request_at += written,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
    true
}

fn drain_connection(connection: &mut Connection) -> bool {
    loop {
        if let Some(event) = connection.pending.take() {
            let is_final = matches!(event, ProgressEvent::Final(_));
            match connection.sender.try_send(event) {
                Ok(()) if is_final => return false,
                Ok(()) => {}
                Err(TrySendError::Full(event)) => {
                    connection.pending = Some(event);
                    connection.blocked = true;
                    return true;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        let Some(position) = connection.buffer.iter().position(|byte| *byte == b'\n') else {
            if connection.buffer.len() > ipc::LOCAL_FRAME_MAX {
                let _ = connection.sender.try_send(ProgressEvent::Error(
                    "management frame exceeds local limit".into(),
                ));
                return false;
            }
            return true;
        };
        if position > ipc::LOCAL_FRAME_MAX {
            let _ = connection.sender.try_send(ProgressEvent::Error(
                "management frame exceeds local limit".into(),
            ));
            return false;
        }
        let line: Vec<u8> = connection.buffer.drain(..=position).collect();
        let line = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).to_string();
        let final_frame = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|value| value.get("id").cloned())
            .is_some();
        let event = if final_frame {
            ProgressEvent::Final(line)
        } else {
            ProgressEvent::Frame(line)
        };
        match connection.sender.try_send(event) {
            Ok(()) => {
                if final_frame {
                    return false;
                }
            }
            Err(TrySendError::Full(event)) => {
                connection.pending = Some(event);
                connection.blocked = true;
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full handler queue must hold the event and stop reading the upstream
    /// socket, not drop or reorder it. The next tick retries the held event.
    #[test]
    fn a_full_progress_queue_holds_the_event_instead_of_dropping_it() {
        let (stream, _writer) = UnixStream::pair().expect("pair");
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let mut connection = Connection {
            stream,
            buffer: Vec::new(),
            request: Vec::new(),
            request_at: 0,
            sender,
            blocked: false,
            pending: None,
        };
        connection.buffer = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n",
        )
        .as_bytes()
        .to_vec();

        assert!(drain_connection(&mut connection));
        assert!(
            connection.blocked,
            "a full queue stops the client from reading more frames"
        );
        assert!(
            matches!(connection.pending, Some(ProgressEvent::Frame(_))),
            "the frame that did not fit is held, not dropped"
        );

        let first = receiver.try_recv().expect("first frame");
        assert!(matches!(first, ProgressEvent::Frame(line) if line.contains("progress")));

        connection.blocked = false;
        assert!(drain_connection(&mut connection));
        let second = receiver.try_recv().expect("the held frame is retried");
        assert!(matches!(second, ProgressEvent::Frame(line) if line.contains("message")));
        assert!(connection.pending.is_none());
    }

    /// An unreachable control channel keeps the SSE shape: open returns a
    /// stream id and the error is a progress event, not an HTTP failure.
    #[test]
    fn an_unreachable_channel_is_reported_as_a_progress_error() {
        let root = std::env::temp_dir().join(format!(
            "svcd-missing-channel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let (sender, receiver) = std::sync::mpsc::sync_channel(4);
        let mut client = StreamingClient::new();
        let _id = client.open(&root, "registry.call", serde_json::json!({}), sender);
        match receiver.try_recv().expect("one error event") {
            ProgressEvent::Error(message) => {
                assert!(message.contains("svcd unreachable"), "{message}")
            }
            _ => panic!("expected a progress error"),
        }
        assert!(client.connections.is_empty(), "no socket was registered");
    }
}
