//! Unix I/O reactor for plugin stdio.
//!
//! Each notify plugin used to own two OS threads (stdout and stderr readers).
//! This module owns all those pipes from one poll loop instead, keeping the
//! process isolation boundary intact while removing the per-plugin thread cost.
//! Windows keeps the existing reader-thread path because anonymous child pipes
//! are not IOCP-ready.

#![cfg(unix)]

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::process::{ChildStderr, ChildStdout};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::ipc::{FrameError, FrameEvent};
use crate::log::{self, LogSink};
use crate::threads;
use crate::wake::Wake;

const POLL_TIMEOUT_MS: i32 = 50;
const READ_CHUNK: usize = 8 * 1024;
const MAX_LOG_LINE: usize = 64 * 1024;

pub struct Reactor {
    tx: Sender<Command>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

enum Command {
    Register(Vec<Source>),
    Unregister(String),
    Shutdown,
}

#[derive(Clone, Copy)]
pub enum Framing {
    Lines,
    Length,
}

enum SourceKind {
    Frames {
        sender: SyncSender<FrameEvent>,
        max: usize,
        wake: Option<Arc<Wake>>,
        framing: Framing,
    },
    Log,
}

struct Source {
    key: String,
    file: File,
    kind: SourceKind,
    buffer: Vec<u8>,
    blocked: bool,
    log: Option<Arc<Mutex<LogSink>>>,
}

impl Reactor {
    pub fn start() -> io::Result<Arc<Self>> {
        let (tx, rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("svcd-reactor".into())
            .stack_size(threads::IO_STACK)
            .spawn(move || run(rx))?;
        Ok(Arc::new(Reactor {
            tx,
            join: Mutex::new(Some(join)),
        }))
    }

    /// Register one notify plugin. A later registration with the same key
    /// replaces the old sources, which is what a restart needs.
    pub fn register(
        &self,
        id: String,
        stdout: ChildStdout,
        stderr: Option<ChildStderr>,
        sender: SyncSender<FrameEvent>,
        wake: Option<Arc<Wake>>,
        log: Option<Arc<Mutex<LogSink>>>,
        max: usize,
        framing: Framing,
    ) -> io::Result<()> {
        let mut sources = vec![Source {
            key: id.clone(),
            file: file(stdout),
            kind: SourceKind::Frames {
                sender,
                max,
                wake,
                framing,
            },
            buffer: Vec::new(),
            blocked: false,
            log: log.clone(),
        }];
        if let Some(stderr) = stderr {
            sources.push(Source {
                key: id,
                file: file(stderr),
                kind: SourceKind::Log,
                buffer: Vec::new(),
                blocked: false,
                log,
            });
        }
        self.tx
            .send(Command::Register(sources))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "reactor is not running"))
    }

    pub fn unregister(&self, id: &str) {
        let _ = self.tx.send(Command::Unregister(id.to_string()));
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        if let Ok(mut join) = self.join.lock() {
            if let Some(join) = join.take() {
                let _ = join.join();
            }
        }
    }
}

fn file<T: IntoRawFd>(source: T) -> File {
    unsafe { File::from_raw_fd(source.into_raw_fd()) }
}

fn run(rx: Receiver<Command>) {
    let mut sources: Vec<Source> = Vec::new();
    loop {
        let mut shutdown = false;
        loop {
            match rx.try_recv() {
                Ok(Command::Register(mut incoming)) => {
                    if let Some(id) = incoming.first().map(|source| source.key.clone()) {
                        sources.retain(|source| source.key != id);
                    }
                    sources.append(&mut incoming);
                }
                Ok(Command::Unregister(id)) => {
                    sources.retain(|source| source.key != id);
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

        // A full per-plugin queue pauses that plugin's reader, not the whole
        // reactor. Retry blocked sources before waiting for more I/O.
        let mut dead = Vec::new();
        for (index, source) in sources.iter_mut().enumerate() {
            if source.blocked {
                source.blocked = false;
                if !drain_lines(source) {
                    dead.push(index);
                }
            }
        }
        for index in dead.into_iter().rev() {
            sources.remove(index);
        }
        let blocked_any = sources.iter().any(|source| source.blocked);
        let mut fds: Vec<libc::pollfd> = sources
            .iter()
            .map(|source| libc::pollfd {
                fd: if source.blocked {
                    -1
                } else {
                    AsRawFd::as_raw_fd(&source.file)
                },
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let timeout = if blocked_any { 10 } else { POLL_TIMEOUT_MS };
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            log::svcd(&format!("svcd: reactor poll failed: {}", error));
            continue;
        }
        if ready == 0 {
            continue;
        }

        let mut index = 0;
        while index < sources.len() {
            if sources[index].blocked {
                index += 1;
                continue;
            }
            let revents = fds[index].revents;
            if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                index += 1;
                continue;
            }
            let keep = read_source(&mut sources[index]);
            if !keep {
                sources.remove(index);
                fds.remove(index);
                continue;
            }
            index += 1;
        }
    }
}

/// Returns false when the source reached EOF or a protocol violation and must
/// be removed. The reactor owns the file; dropping removes it and closes the
/// descriptor.
fn read_source(source: &mut Source) -> bool {
    let mut chunk = [0u8; READ_CHUNK];
    match source.file.read(&mut chunk) {
        Ok(0) => {
            if let SourceKind::Frames { sender, .. } = &source.kind {
                let _ = sender.try_send(FrameEvent::Closed);
            }
            false
        }
        Ok(read) => {
            source.buffer.extend_from_slice(&chunk[..read]);
            drain_lines(source)
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => true,
        Err(error) => {
            if let SourceKind::Frames { sender, .. } = &source.kind {
                let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::Io(error))));
            }
            false
        }
    }
}

fn drain_lines(source: &mut Source) -> bool {
    if let SourceKind::Frames { framing, .. } = &source.kind {
        if matches!(framing, Framing::Length) {
            return drain_length_frames(source);
        }
    }
    loop {
        let Some(position) = source.buffer.iter().position(|byte| *byte == b'\n') else {
            let max = match &source.kind {
                SourceKind::Frames { max, .. } => *max,
                SourceKind::Log => MAX_LOG_LINE,
            };
            if source.buffer.len() > max {
                if let SourceKind::Frames { sender, .. } = &source.kind {
                    let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max })));
                }
                return false;
            }
            return true;
        };
        match &source.kind {
            SourceKind::Frames {
                sender, max, wake, ..
            } => {
                let line = &source.buffer[..position];
                if line.len() > *max {
                    let _ =
                        sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max: *max })));
                    return false;
                }
                let text = match std::str::from_utf8(line) {
                    Ok(text) => text.to_string(),
                    Err(_) => {
                        let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::InvalidUtf8)));
                        return false;
                    }
                };
                match sender.try_send(FrameEvent::Frame(Ok(text))) {
                    Ok(()) => {
                        source.buffer.drain(..=position);
                        if let Some(wake) = wake {
                            wake.notify();
                        }
                    }
                    Err(TrySendError::Full(_)) => {
                        // Keep the line in the buffer; the receiver will drain
                        // it and the reactor retries this source.
                        source.blocked = true;
                        return true;
                    }
                    Err(TrySendError::Disconnected(_)) => return false,
                }
            }
            SourceKind::Log => {
                let line: Vec<u8> = source.buffer.drain(..=position).collect();
                let line = &line[..line.len().saturating_sub(1)];
                log_line(&source.log, &String::from_utf8_lossy(line));
            }
        }
    }
}

fn drain_length_frames(source: &mut Source) -> bool {
    loop {
        if source.buffer.len() < 4 {
            return true;
        }
        let length = u32::from_be_bytes([
            source.buffer[0],
            source.buffer[1],
            source.buffer[2],
            source.buffer[3],
        ]) as usize;
        let SourceKind::Frames {
            sender, max, wake, ..
        } = &source.kind
        else {
            return false;
        };
        if length > *max {
            let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max: *max })));
            return false;
        }
        if source.buffer.len() < 4 + length {
            if source.buffer.len() > *max + 4 {
                let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge { max: *max })));
                return false;
            }
            return true;
        }
        let text = match std::str::from_utf8(&source.buffer[4..4 + length]) {
            Ok(text) => text.to_string(),
            Err(_) => {
                let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::InvalidUtf8)));
                return false;
            }
        };
        match sender.try_send(FrameEvent::Frame(Ok(text))) {
            Ok(()) => {
                source.buffer.drain(..4 + length);
                if let Some(wake) = wake {
                    wake.notify();
                }
            }
            Err(TrySendError::Full(_)) => {
                source.blocked = true;
                return true;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn log_line(log: &Option<Arc<Mutex<LogSink>>>, line: &str) {
    if let Some(sink) = log {
        if let Ok(mut sink) = sink.lock() {
            let _ = sink.line(line);
        }
    }
}
