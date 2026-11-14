//! Unix management-channel reactor.
//!
//! One poll thread serves every local CLI/HTTP/MCP control connection. Replies
//! arrive through the daemon's `PendingCall.reply`; a small pipe wakes the
//! reactor so responses do not need a polling tick.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use crate::channel::{PendingCall, Reply};
use crate::ipc;
use crate::rpc;
use crate::threads;
use crate::wake::Wake;

struct Connection {
    stream: UnixStream,
    buffer: Vec<u8>,
    out: Vec<u8>,
    out_at: usize,
    pending: Option<Receiver<String>>,
    close_after_write: bool,
}

pub fn spawn(listener: UnixListener, tx: Sender<PendingCall>, wake: Arc<Wake>) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let (notify_rx, notify_tx) = UnixStream::pair()?;
    notify_rx.set_nonblocking(true)?;
    notify_tx.set_nonblocking(true)?;
    let notify = Arc::new(notify_tx);
    thread::Builder::new()
        .name("svcd-channel-reactor".into())
        .stack_size(threads::IO_STACK)
        .spawn(move || run(listener, tx, wake, notify, notify_rx))
        .map(|_| ())
}

fn run(
    listener: UnixListener,
    tx: Sender<PendingCall>,
    wake: Arc<Wake>,
    notify: Arc<UnixStream>,
    mut notify_rx: UnixStream,
) {
    let mut connections: HashMap<u64, Connection> = HashMap::new();
    let mut next_id = 1u64;
    loop {
        if drain_notifications(&mut notify_rx, &mut connections) {
            continue;
        }

        let mut descriptors = Vec::with_capacity(connections.len() + 2);
        let mut keys = Vec::with_capacity(connections.len() + 2);
        descriptors.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        keys.push(0);
        descriptors.push(libc::pollfd {
            fd: notify_rx.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        keys.push(1);
        for (id, connection) in &connections {
            let mut events = 0;
            if connection.pending.is_none() && connection.out.is_empty() {
                events |= libc::POLLIN;
            }
            if !connection.out.is_empty() {
                events |= libc::POLLOUT;
            }
            descriptors.push(libc::pollfd {
                fd: connection.stream.as_raw_fd(),
                events,
                revents: 0,
            });
            keys.push(*id);
        }

        let ready = unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                100,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                eprintln!("svcd: channel reactor poll failed: {}", error);
            }
            continue;
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            drain_notifications(&mut notify_rx, &mut connections);
        }

        if descriptors[0].revents & libc::POLLIN != 0 {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(true);
                        let id = next_id;
                        next_id = next_id.saturating_add(1);
                        connections.insert(
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
                        eprintln!("svcd: channel accept failed: {}", error);
                        break;
                    }
                }
            }
        }

        for index in 2..descriptors.len() {
            let id = keys[index];
            let Some(connection) = connections.get_mut(&id) else {
                continue;
            };
            let revents = descriptors[index].revents;
            if revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                connections.remove(&id);
                continue;
            }
            if revents & libc::POLLIN != 0
                && connection.pending.is_none()
                && connection.out.is_empty()
            {
                let mut chunk = [0u8; 8 * 1024];
                match connection.stream.read(&mut chunk) {
                    Ok(0) => {
                        connections.remove(&id);
                        continue;
                    }
                    Ok(read) => {
                        connection.buffer.extend_from_slice(&chunk[..read]);
                        if !drain_requests(connection, &tx, &wake, &notify) {
                            connections.remove(&id);
                            continue;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        connections.remove(&id);
                        continue;
                    }
                }
            }

            let Some(connection) = connections.get_mut(&id) else {
                continue;
            };
            if !connection.out.is_empty() {
                match connection
                    .stream
                    .write(&connection.out[connection.out_at..])
                {
                    Ok(0) => {
                        connections.remove(&id);
                        continue;
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
        }
    }
}

fn drain_requests(
    connection: &mut Connection,
    tx: &Sender<PendingCall>,
    wake: &Arc<Wake>,
    notify: &Arc<UnixStream>,
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
                reply: Reply::with_notify(sender, notify.clone()),
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

fn drain_notifications(
    notify_rx: &mut UnixStream,
    connections: &mut HashMap<u64, Connection>,
) -> bool {
    let mut any = false;
    let mut drain = [0u8; 64];
    loop {
        match notify_rx.read(&mut drain) {
            Ok(0) => break,
            Ok(_) => any = true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    if !any {
        return false;
    }
    for connection in connections.values_mut() {
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
    true
}
