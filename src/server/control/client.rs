//! Outbound management streaming client, owned by the server loop.
//!
//! A streaming `registry.call` (MCP progress) opens one management-channel
//! connection. The server loop multiplexes those sockets with everything else;
//! progress lines go into the owning MCP stream handler's bounded queue, and a
//! full queue stops reading the upstream socket instead of growing memory.

use std::collections::BTreeMap;
use std::io::{self, Read};
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

    /// Open one streaming management request. The connection is nonblocking
    /// after the small request frame is written; its progress lines are
    /// delivered to `sender`.
    pub fn open(
        &mut self,
        config: &Path,
        method: &str,
        params: Value,
        sender: SyncSender<ProgressEvent>,
    ) -> io::Result<u64> {
        let token = channel::read_token(config)?;
        let mut stream = UnixStream::connect(config.join("svcd.sock"))?;
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
            "auth": token,
        })
        .to_string();
        if ipc::write_frame_limit(&mut stream, &line, ipc::LOCAL_FRAME_MAX).is_err() {
            return Err(io::Error::other("cannot write management request"));
        }
        stream.set_nonblocking(true)?;
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.connections.insert(
            id,
            Connection {
                stream,
                buffer: Vec::new(),
                sender,
                blocked: false,
                pending: None,
            },
        );
        Ok(id)
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
    /// `ready`. A blocked stream is omitted so a full handler queue
    /// backpressures the upstream socket.
    pub fn poll_sources(&self, out: &mut Vec<(RawFd, i16)>, keys: &mut Vec<u64>) {
        for (id, connection) in &self.connections {
            if connection.blocked {
                continue;
            }
            out.push((connection.stream.as_raw_fd(), libc::POLLIN));
            keys.push(*id);
        }
    }

    pub fn ready(&mut self, id: u64, revents: i16) {
        let Some(connection) = self.connections.get_mut(&id) else {
            return;
        };
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
        if revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            self.connections.remove(&id);
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
}
