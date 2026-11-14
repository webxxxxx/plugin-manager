//! Unix reactor for outbound management-channel streaming calls.
//!
//! HTTP MCP progress used to spend one thread per active stream waiting on the
//! blocking `channel::request_stream`. This reactor multiplexes those upstream
//! sockets instead: one poll thread, one fd per stream, bounded per-stream
//! queues.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;

use crate::channel;
use crate::ipc;

static NEXT: AtomicU64 = AtomicU64::new(1);

pub enum Event {
    Frame(String),
    Final(String),
    Error(String),
}

pub struct ClientReactor {
    tx: Sender<Command>,
    notify: Arc<UnixStream>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

enum Command {
    Open {
        id: u64,
        config: std::path::PathBuf,
        method: String,
        params: Value,
        sender: SyncSender<Event>,
    },
    Cancel(u64),
    Shutdown,
}

struct Connection {
    stream: UnixStream,
    buffer: Vec<u8>,
    sender: SyncSender<Event>,
    blocked: bool,
    pending: Option<Event>,
}

impl ClientReactor {
    pub fn start() -> io::Result<Arc<Self>> {
        let (tx, rx) = mpsc::channel();
        let (notify_rx, notify) = UnixStream::pair()?;
        notify.set_nonblocking(true)?;
        notify_rx.set_nonblocking(true)?;
        let join = thread::Builder::new()
            .name("svcd-channel-client".into())
            .stack_size(128 * 1024)
            .spawn(move || run(rx, notify_rx))?;
        Ok(Arc::new(ClientReactor {
            tx,
            notify: Arc::new(notify),
            join: Mutex::new(Some(join)),
        }))
    }

    pub fn open(
        &self,
        config: &Path,
        method: &str,
        params: Value,
        sender: SyncSender<Event>,
    ) -> io::Result<u64> {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        self.tx
            .send(Command::Open {
                id,
                config: config.to_path_buf(),
                method: method.to_string(),
                params,
                sender,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "client reactor stopped"))?;
        notify(&self.notify);
        Ok(id)
    }

    pub fn cancel(&self, id: u64) {
        let _ = self.tx.send(Command::Cancel(id));
        notify(&self.notify);
    }
}

impl Drop for ClientReactor {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        notify(&self.notify);
        if let Ok(mut join) = self.join.lock() {
            if let Some(join) = join.take() {
                let _ = join.join();
            }
        }
    }
}

fn notify(stream: &UnixStream) {
    let _ = (&*stream).write(&[1]);
}

fn run(rx: Receiver<Command>, mut notify_rx: UnixStream) {
    let mut connections: BTreeMap<u64, Connection> = BTreeMap::new();
    loop {
        let mut shutdown = false;
        loop {
            match rx.try_recv() {
                Ok(Command::Open {
                    id,
                    config,
                    method,
                    params,
                    sender,
                }) => {
                    if let Some(connection) = open_connection(id, &config, &method, params, sender)
                    {
                        connections.insert(id, connection);
                    }
                }
                Ok(Command::Cancel(id)) => {
                    connections.remove(&id);
                }
                Ok(Command::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            return;
        }

        let mut dead = Vec::new();
        for (id, connection) in &mut connections {
            if connection.blocked {
                connection.blocked = false;
                if !drain_connection(connection) {
                    dead.push(*id);
                }
            }
        }
        for id in dead {
            connections.remove(&id);
        }

        let mut descriptors = Vec::with_capacity(connections.len() + 1);
        let mut keys = Vec::with_capacity(connections.len() + 1);
        descriptors.push(libc::pollfd {
            fd: notify_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        keys.push(0);
        for (id, connection) in &connections {
            descriptors.push(libc::pollfd {
                fd: if connection.blocked {
                    -1
                } else {
                    connection.stream.as_raw_fd()
                },
                events: libc::POLLIN,
                revents: 0,
            });
            keys.push(*id);
        }
        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                50,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                eprintln!("svcd: channel client reactor poll failed: {}", error);
            }
            continue;
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            let mut drain = [0u8; 64];
            let _ = notify_rx.read(&mut drain);
        }
        for index in 1..descriptors.len() {
            if descriptors[index].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                continue;
            }
            let id = keys[index];
            let Some(connection) = connections.get_mut(&id) else {
                continue;
            };
            if connection.blocked {
                continue;
            }
            let mut chunk = [0u8; 8 * 1024];
            match connection.stream.read(&mut chunk) {
                Ok(0) => {
                    connections.remove(&id);
                }
                Ok(read) => {
                    connection.buffer.extend_from_slice(&chunk[..read]);
                    if !drain_connection(connection) {
                        connections.remove(&id);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => {
                    connections.remove(&id);
                }
            }
        }
    }
}

fn open_connection(
    _id: u64,
    config: &Path,
    method: &str,
    params: Value,
    sender: SyncSender<Event>,
) -> Option<Connection> {
    let token = match channel::read_token(config) {
        Ok(token) => token,
        Err(error) => {
            let _ = sender.try_send(Event::Error(format!("svcd unreachable: {}", error)));
            return None;
        }
    };
    let mut stream = match UnixStream::connect(config.join("svcd.sock")) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = sender.try_send(Event::Error(format!("svcd unreachable: {}", error)));
            return None;
        }
    };
    let line = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
        "auth": token,
    })
    .to_string();
    if ipc::write_frame_limit(&mut stream, &line, ipc::LOCAL_FRAME_MAX).is_err() {
        let _ = sender.try_send(Event::Error("cannot write management request".into()));
        return None;
    }
    let _ = stream.set_nonblocking(true);
    Some(Connection {
        stream,
        buffer: Vec::new(),
        sender,
        blocked: false,
        pending: None,
    })
}

fn drain_connection(connection: &mut Connection) -> bool {
    loop {
        if let Some(event) = connection.pending.take() {
            let is_final = matches!(event, Event::Final(_));
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
                let _ = connection
                    .sender
                    .try_send(Event::Error("management frame exceeds local limit".into()));
                return false;
            }
            return true;
        };
        if position > ipc::LOCAL_FRAME_MAX {
            let _ = connection
                .sender
                .try_send(Event::Error("management frame exceeds local limit".into()));
            return false;
        }
        let line: Vec<u8> = connection.buffer.drain(..=position).collect();
        let line = String::from_utf8_lossy(&line[..line.len().saturating_sub(1)]).to_string();
        let final_frame = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|value| value.get("id").cloned())
            .is_some();
        let event = if final_frame {
            Event::Final(line)
        } else {
            Event::Frame(line)
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
