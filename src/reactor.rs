//! Unix I/O reactor for plugin stdio.
//!
//! Each notify plugin used to own two OS threads (stdout and stderr readers).
//! This module owns all those pipes from one poll set instead, keeping the
//! process isolation boundary intact while removing the per-plugin thread cost.
//! Windows keeps the existing reader-thread path because anonymous child pipes
//! are not IOCP-ready.
//!
//! The poll set has no thread of its own: the supervisor loop owns it and
//! drives one pass with Reactor::poll_once on its own thread, so plugin stdio
//! costs no daemon thread (#126). Producers (a slot starting a plugin, the raw
//! hub queueing a frame) only send commands and poke the self-pipe.
//!
//! Protocol-3 raw plugins are part of the same poll set: their stdout carries
//! `u32 payload_len + u32 stream_id + payload` frames and their stdin is fed
//! from a bounded per-plugin queue, so a raw plugin costs no threads at all.

#![cfg(unix)]

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use crate::ipc::{FrameError, FrameEvent};
use crate::log::{self, LogSink};
use crate::ipc::{RawFailure, RawStopSink};
use crate::wake::Wake;

const READ_CHUNK: usize = 8 * 1024;
const MAX_LOG_LINE: usize = 64 * 1024;
/// Protocol-3 raw frame header: `u32 payload_len (BE) + u32 stream_id (BE)`.
const RAW_HEADER: usize = 8;
/// Raw stdout is read in large non-blocking bursts: a 512 KiB plugin frame
/// should not cost 64 poll passes.
const RAW_READ_CHUNK: usize = 64 * 1024;
/// Bounds how much one plugin can move per poll pass: a pass that drains one
/// plugin greedily starves every other plugin's stdin.
const RAW_READ_BUDGET: usize = 256 * 1024;
/// Keep a drained raw read buffer from pinning a large frame allocation.
const RAW_BUFFER_SHRINK: usize = 64 * 1024;
/// Bytes one poll pass writes to a single plugin stdin. Bounding this keeps a
/// fast producer from holding the queue lock or starving other sources.
const RAW_WRITE_BUDGET: usize = 256 * 1024;

pub struct Reactor {
    tx: Sender<Command>,
    /// Self-pipe: commands and raw payloads wake the poll immediately instead
    /// of waiting for the timeout.
    wake: UnixStream,
    /// The poll set. The supervisor loop drives it; producers only enqueue
    /// commands through tx and poke wake.
    poll_set: Mutex<PollSet>,
}

struct PollSet {
    rx: Receiver<Command>,
    wake: UnixStream,
    sources: Vec<Source>,
}

enum Command {
    Register(Vec<Source>),
    Unregister(String),
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
    /// Protocol-3 plugin stdin: frames queued by the hub, written when the
    /// pipe reports writable.
    RawStdin {
        queue: Arc<RawOutQueue>,
    },
    /// Protocol-3 plugin stdout: length-prefixed raw frames.
    RawStdout {
        max_frame: usize,
    },
}

/// Why a source must leave the poll set.
enum Stop {
    /// Drop only this source; the process may still be running (a log pipe
    /// ended, or a JSON-RPC stdout hit a protocol violation).
    Single,
    /// The plugin's stdio is unusable: drop every source of this plugin and
    /// report the reason to the raw hub.
    Plugin(RawFailure),
}

struct Source {
    key: String,
    file: File,
    kind: SourceKind,
    buffer: Vec<u8>,
    blocked: bool,
    log: Option<Arc<Mutex<LogSink>>>,
    /// Set for protocol-3 sources; the reactor reports the stop reason there
    /// together with the transport generation it registered.
    hub: Option<(Weak<dyn RawStopSink>, u64)>,
    stop: Option<Stop>,
}

impl Source {
    fn new(key: String, file: File, kind: SourceKind) -> Self {
        Self {
            key,
            file,
            kind,
            buffer: Vec::new(),
            blocked: false,
            log: None,
            hub: None,
            stop: None,
        }
    }

    fn with_log(mut self, log: Option<Arc<Mutex<LogSink>>>) -> Self {
        self.log = log;
        self
    }

    fn with_plugin(mut self, hub: Weak<dyn RawStopSink>, generation: u64) -> Self {
        self.hub = Some((hub, generation));
        self
    }
}

/// Bounded queue of complete raw frames waiting for one plugin's stdin.
///
/// The hub pushes frames from a server-loop thread; the supervisor loop's
/// plugin-stdio poll drains them on `POLLOUT`. Both sides stay non-blocking,
/// so a slow plugin costs queue slots (reported as `writer_queue_full`)
/// instead of a blocked thread.
pub struct RawOutQueue {
    state: Mutex<RawOutState>,
    wake: UnixStream,
    capacity: usize,
}

struct RawOutState {
    frames: VecDeque<Vec<u8>>,
    current: Vec<u8>,
    at: usize,
}

impl RawOutQueue {
    fn new(capacity: usize, wake: UnixStream) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(RawOutState {
                frames: VecDeque::new(),
                current: Vec::new(),
                at: 0,
            }),
            wake,
            capacity: capacity.max(1),
        })
    }

    /// Queue one complete frame (header + payload) for the plugin's stdin.
    pub fn push(&self, frame: Vec<u8>) -> Result<(), RawFailure> {
        let was_empty = {
            let mut state = self.state.lock().map_err(|_| RawFailure::WriterClosed)?;
            if state.frames.len() >= self.capacity {
                return Err(RawFailure::WriterQueueFull);
            }
            let idle = state.frames.is_empty() && state.at >= state.current.len();
            state.frames.push_back(frame);
            idle
        };
        // Only the idle -> busy transition needs a wake-up: otherwise the pipe
        // is already polled for POLLOUT.
        if was_empty {
            let mut handle = &self.wake;
            let _ = handle.write(&[1]);
        }
        Ok(())
    }

    pub fn has_pending(&self) -> bool {
        match self.state.lock() {
            Ok(state) => !state.frames.is_empty() || state.at < state.current.len(),
            Err(_) => false,
        }
    }

    /// Write what the pipe accepts. `Ok(())` means "poll again later"; an
    /// error means the plugin's stdin is gone.
    fn flush(&self, file: &mut File) -> io::Result<()> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return Err(io::Error::other("raw write queue poisoned")),
        };
        let mut written = 0usize;
        loop {
            if state.at >= state.current.len() {
                match state.frames.pop_front() {
                    Some(frame) => {
                        state.current = frame;
                        state.at = 0;
                        continue;
                    }
                    None => return Ok(()),
                }
            }
            let rest = &state.current[state.at..];
            match file.write(rest) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    state.at += count;
                    written += count;
                    if written >= RAW_WRITE_BUDGET {
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

thread_local! {
    /// Set on the supervisor loop only. That loop owns plugin stdio, so a
    /// synchronous wait on its thread (an install hook, a status hook) polls
    /// one pass to keep the plugin pipes moving. Other threads have no binding.
    static WAITING_LOOP: RefCell<Option<Weak<Reactor>>> = const { RefCell::new(None) };
}

/// Bind `reactor` as this thread's poll set so synchronous waits can poll it.
/// Only the loop that owns the reactor may bind it.
pub fn bind_waiting_loop(reactor: &Arc<Reactor>) {
    WAITING_LOOP.with(|slot| *slot.borrow_mut() = Some(Arc::downgrade(reactor)));
}

/// One non-blocking pass over the poll set bound to this thread. A thread that
/// owns no reactor (a worker, a test, Windows) does nothing.
pub fn poll_waiting_loop() {
    let reactor = WAITING_LOOP.with(|slot| slot.borrow().as_ref().and_then(Weak::upgrade));
    if let Some(reactor) = reactor {
        reactor.poll_once(Duration::ZERO);
    }
}

impl Reactor {
    /// Build the poll set and its handle. No thread is started: the caller's
    /// loop owns the polling, which is the supervisor loop in the daemon.
    pub fn start() -> io::Result<Arc<Self>> {
        let (wake_rx, wake) = UnixStream::pair()?;
        wake_rx.set_nonblocking(true)?;
        wake.set_nonblocking(true)?;
        let (tx, rx) = mpsc::channel();
        Ok(Arc::new(Reactor {
            tx,
            wake,
            poll_set: Mutex::new(PollSet {
                rx,
                wake: wake_rx,
                sources: Vec::new(),
            }),
        }))
    }

    /// A writable clone of the self-pipe. A loop that owns this reactor attaches
    /// it to its wake latch, so a management request interrupts the poll instead
    /// of waiting for the timeout.
    pub fn interrupt_handle(&self) -> io::Result<UnixStream> {
        self.wake.try_clone()
    }

    /// Drive one poll pass on the caller's thread. Registration changes queued
    /// since the last pass are applied first; `timeout` bounds the wait, and
    /// `Duration::ZERO` turns the pass into a non-blocking drain for a caller
    /// that is already busy (a synchronous start/stop wait).
    pub fn poll_once(&self, timeout: Duration) {
        let mut poll_set = match self.poll_set.lock() {
            Ok(poll_set) => poll_set,
            Err(poisoned) => poisoned.into_inner(),
        };
        poll_set.apply_commands();
        poll_set.poll(timeout);
    }

    /// Register one notify plugin. A later registration with the same key
    /// replaces the old sources, which is what a restart needs.
    ///
    /// `stdout`/`stderr` come from [`crate::stdio`]: the pipe layer created
    /// them and the caller asked for the ends. The poll set owns them from here
    /// on; they are blocking, and one read per readiness event is what keeps
    /// the supervisor loop from ever sitting in `read`.
    pub fn register(
        &self,
        id: String,
        stdout: File,
        stderr: Option<File>,
        sender: SyncSender<FrameEvent>,
        wake: Option<Arc<Wake>>,
        log: Option<Arc<Mutex<LogSink>>>,
        max: usize,
        framing: Framing,
    ) -> io::Result<()> {
        let mut sources = vec![
            Source::new(
                id.clone(),
                stdout,
                SourceKind::Frames {
                    sender,
                    max,
                    wake,
                    framing,
                },
            )
            .with_log(log.clone()),
        ];
        if let Some(stderr) = stderr {
            sources.push(Source::new(id, stderr, SourceKind::Log).with_log(log));
        }
        self.send(Command::Register(sources))
    }

    /// Register an exec-ready service's stdout/stderr as log-only sources.
    /// These have no protocol channel, but both pipes still belong to the
    /// reactor so the daemon does not need two OS threads per service.
    pub fn register_log(
        &self,
        id: String,
        stdout: File,
        stderr: Option<File>,
        log: Option<Arc<Mutex<LogSink>>>,
    ) -> io::Result<()> {
        let mut sources = vec![
            Source::new(id.clone(), stdout, SourceKind::Log).with_log(log.clone()),
        ];
        if let Some(stderr) = stderr {
            sources.push(Source::new(id, stderr, SourceKind::Log).with_log(log));
        }
        self.send(Command::Register(sources))
    }

    /// Register a protocol-3 plugin's stdin/stdout/stderr with the poll set
    /// and return the queue the hub feeds frames into.
    ///
    /// `stdin`/`stdout` are already non-blocking (see [`crate::stdio::poll_read`]).
    /// The raw transport drains in bursts, so `WouldBlock` is its "empty"
    /// signal; `enlarge_pipe` gives one poll pass room to hand a whole frame
    /// to the plugin.
    pub fn register_raw(
        &self,
        id: String,
        stdin: File,
        stdout: File,
        stderr: Option<File>,
        log: Option<Arc<Mutex<LogSink>>>,
        hub: Weak<dyn RawStopSink>,
        generation: u64,
        max_frame: usize,
        capacity: usize,
    ) -> io::Result<Arc<RawOutQueue>> {
        enlarge_pipe(&stdin);
        let queue = RawOutQueue::new(capacity, self.wake.try_clone()?);
        let mut sources = vec![
            Source::new(
                id.clone(),
                stdout,
                SourceKind::RawStdout { max_frame },
            )
            .with_log(log.clone())
            .with_plugin(hub.clone(), generation),
            Source::new(
                id.clone(),
                stdin,
                SourceKind::RawStdin {
                    queue: queue.clone(),
                },
            )
            .with_plugin(hub, generation),
        ];
        if let Some(stderr) = stderr {
            sources.push(Source::new(id, stderr, SourceKind::Log).with_log(log));
        }
        self.send(Command::Register(sources))?;
        Ok(queue)
    }

    pub fn unregister(&self, id: &str) {
        let _ = self.send(Command::Unregister(id.to_string()));
    }

    fn send(&self, command: Command) -> io::Result<()> {
        self.tx
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "reactor is not running"))?;
        let mut handle = &self.wake;
        let _ = handle.write(&[1]);
        Ok(())
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        // The owning loop is going away; dropping the sources closes every
        // plugin pipe this reactor held.
        if let Ok(mut poll_set) = self.poll_set.lock() {
            poll_set.sources.clear();
        }
    }
}

/// Ask for a larger pipe so one poll pass can hand a whole raw frame to the
/// plugin instead of trickling it in 64 KiB slices. Best effort: the kernel
/// caps this at `pipe-max-size` and the reactor works either way. Only Linux
/// (and Android) expose `F_SETPIPE_SZ`; elsewhere the default 64 KiB stands.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn enlarge_pipe(file: &File) {
    const PIPE_BYTES: libc::c_int = 512 * 1024;
    unsafe {
        libc::fcntl(file.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_BYTES);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn enlarge_pipe(_file: &File) {}

impl PollSet {
    /// Apply every registration change queued since the last pass.
    fn apply_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Command::Register(mut incoming)) => {
                    if let Some(id) = incoming.first().map(|source| source.key.clone()) {
                        self.sources.retain(|source| source.key != id);
                    }
                    self.sources.append(&mut incoming);
                }
                Ok(Command::Unregister(id)) => self.sources.retain(|source| source.key != id),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // No producer can reach these pipes any more.
                    self.sources.clear();
                    break;
                }
            }
        }
    }

    /// One poll pass. The caller's loop owns this call, so the wait is the
    /// supervisor loop's only blocking point for plugin stdio.
    fn poll(&mut self, timeout: Duration) {
        let sources = &mut self.sources;
        // A full per-plugin queue pauses that plugin's reader, not the whole
        // poll set. Retry blocked sources before waiting for more I/O.
        for source in sources.iter_mut() {
            if !source.blocked {
                continue;
            }
            source.blocked = false;
            resume(source);
        }
        retire(sources);

        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(sources.len() + 1);
        fds.push(libc::pollfd {
            fd: self.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        for source in sources.iter() {
            let events = poll_events(source);
            fds.push(libc::pollfd {
                fd: if events == 0 {
                    -1
                } else {
                    source.file.as_raw_fd()
                },
                events,
                revents: 0,
            });
        }
        let blocked_any = sources.iter().any(|source| source.blocked);
        let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        // A blocked source is retried on the next pass, so it must not wait for
        // the caller's full timeout. A caller that is already busy passes 0.
        let timeout_ms = if blocked_any {
            timeout_ms.min(10)
        } else {
            timeout_ms
        };
        let ready = unsafe {
            libc::poll(
                fds.as_mut_ptr(),
                fds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                log::svcd(&format!("svcd: reactor poll failed: {}", error));
            }
            return;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            drain_wake(&self.wake);
        }

        // Feed plugin stdin before collecting plugin stdout: a plugin can only
        // produce output for input it already has, and a pass that greedily
        // drains every stdout pipe starves all the stdin pipes.
        for index in 0..sources.len() {
            if !matches!(sources[index].kind, SourceKind::RawStdin { .. }) {
                continue;
            }
            let revents = fds[index + 1].revents;
            if revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                write_raw(&mut sources[index]);
            }
        }
        for index in 0..sources.len() {
            if matches!(sources[index].kind, SourceKind::RawStdin { .. }) {
                continue;
            }
            let revents = fds[index + 1].revents;
            if revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0 {
                read_source(&mut sources[index]);
            }
        }
        retire(sources);
    }
}

fn drain_wake(wake: &UnixStream) {
    let mut handle = wake;
    let mut buffer = [0u8; 64];
    loop {
        match handle.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

fn poll_events(source: &Source) -> i16 {
    if source.blocked {
        return 0;
    }
    match &source.kind {
        SourceKind::RawStdin { queue } => {
            if queue.has_pending() {
                libc::POLLOUT
            } else {
                0
            }
        }
        _ => libc::POLLIN,
    }
}

/// Drop every source marked dead and tell the raw hub which plugins lost
/// their transport.
fn retire(sources: &mut Vec<Source>) {
    if !sources.iter().any(|source| source.stop.is_some()) {
        return;
    }
    let mut kept = Vec::with_capacity(sources.len());
    let mut closing: Vec<(String, Option<(Weak<dyn RawStopSink>, u64)>, RawFailure)> = Vec::new();
    let mut closed = HashSet::new();
    for source in sources.drain(..) {
        match source.stop {
            None => kept.push(source),
            Some(Stop::Single) => {}
            Some(Stop::Plugin(reason)) => {
                if closed.insert(source.key.clone()) {
                    closing.push((source.key.clone(), source.hub.clone(), reason));
                }
            }
        }
    }
    kept.retain(|source| !closed.contains(&source.key));
    *sources = kept;
    for (key, hub, reason) in closing {
        let Some((hub, generation)) = hub else { continue };
        let Some(hub) = hub.upgrade() else { continue };
        hub.note_transport_failure(reason, &key);
        hub.unregister_generation(&key, generation);
    }
}

fn stop_for(source: &Source, reason: RawFailure) -> Stop {
    match &source.kind {
        SourceKind::RawStdin { .. } | SourceKind::RawStdout { .. } => Stop::Plugin(reason),
        _ => Stop::Single,
    }
}

/// A source that was paused because its consumer queue was full.
fn resume(source: &mut Source) {
    if matches!(source.kind, SourceKind::RawStdin { .. }) {
        write_raw(source);
        return;
    }
    if !drain_source(source) {
        source.stop = Some(stop_for(source, RawFailure::FrameTooLarge));
    }
}

fn write_raw(source: &mut Source) {
    let SourceKind::RawStdin { queue } = &source.kind else {
        return;
    };
    let queue = queue.clone();
    if queue.flush(&mut source.file).is_err() {
        source.stop = Some(Stop::Plugin(RawFailure::PluginWriteError));
    }
}

/// Returns false when the source hit EOF or a protocol violation and must be
/// removed. The reactor owns the file; dropping it closes the descriptor.
fn read_source(source: &mut Source) {
    if matches!(source.kind, SourceKind::RawStdout { .. }) {
        read_raw(source);
        return;
    }
    read_stream(source);
}

/// Drain a non-blocking raw stdout until the kernel reports it empty. The
/// budget bounds how much one plugin can move per poll pass.
fn read_raw(source: &mut Source) {
    let mut chunk = vec![0u8; RAW_READ_CHUNK];
    let mut budget = RAW_READ_BUDGET;
    loop {
        match source.file.read(&mut chunk) {
            Ok(0) => {
                source.stop = Some(Stop::Plugin(RawFailure::PluginEof));
                return;
            }
            Ok(read) => {
                source.buffer.extend_from_slice(&chunk[..read]);
                if !drain_raw_frames(source) {
                    source.stop = Some(Stop::Plugin(RawFailure::FrameTooLarge));
                    return;
                }
                budget = budget.saturating_sub(read);
                if budget == 0 {
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                source.stop = Some(Stop::Plugin(RawFailure::PluginEof));
                return;
            }
        }
    }
}

fn read_stream(source: &mut Source) {
    let mut chunk = [0u8; READ_CHUNK];
    match source.file.read(&mut chunk) {
        Ok(0) => {
            if let SourceKind::Frames { sender, .. } = &source.kind {
                let _ = sender.try_send(FrameEvent::Closed);
            }
            source.stop = Some(stop_for(source, RawFailure::PluginEof));
        }
        Ok(read) => {
            source.buffer.extend_from_slice(&chunk[..read]);
            if !drain_source(source) {
                source.stop = Some(stop_for(source, RawFailure::FrameTooLarge));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
        Err(error) => {
            if let SourceKind::Frames { sender, .. } = &source.kind {
                let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::Io(error))));
            }
            source.stop = Some(stop_for(source, RawFailure::PluginEof));
        }
    }
}

fn drain_source(source: &mut Source) -> bool {
    match &source.kind {
        SourceKind::RawStdout { .. } => drain_raw_frames(source),
        SourceKind::RawStdin { .. } => true,
        _ => drain_lines(source),
    }
}

fn drain_raw_frames(source: &mut Source) -> bool {
    let SourceKind::RawStdout { max_frame } = &source.kind else {
        return false;
    };
    let max_frame = *max_frame;
    loop {
        if source.buffer.len() < RAW_HEADER {
            break;
        }
        let length = u32::from_be_bytes([
            source.buffer[0],
            source.buffer[1],
            source.buffer[2],
            source.buffer[3],
        ]) as usize;
        let stream = u32::from_be_bytes([
            source.buffer[4],
            source.buffer[5],
            source.buffer[6],
            source.buffer[7],
        ]);
        if length > max_frame {
            return false;
        }
        if source.buffer.len() < RAW_HEADER + length {
            break;
        }
        // The common case is one frame per pass: hand the buffer over instead
        // of allocating and copying the payload out of it.
        if source.buffer.len() == RAW_HEADER + length {
            let mut frame = std::mem::take(&mut source.buffer);
            frame.drain(..RAW_HEADER);
            if let Some(hub) = source.hub.as_ref().and_then(|(hub, _)| hub.upgrade()) {
                hub.deliver(&source.key, stream, frame);
            }
            continue;
        }
        let payload: Vec<u8> = source
            .buffer
            .drain(..RAW_HEADER + length)
            .skip(RAW_HEADER)
            .collect();
        if let Some(hub) = source.hub.as_ref().and_then(|(hub, _)| hub.upgrade()) {
            hub.deliver(&source.key, stream, payload);
        }
    }
    if source.buffer.is_empty() && source.buffer.capacity() > RAW_BUFFER_SHRINK {
        source.buffer.shrink_to(RAW_BUFFER_SHRINK);
    }
    true
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
                SourceKind::RawStdin { .. } | SourceKind::RawStdout { .. } => MAX_LOG_LINE,
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
            SourceKind::RawStdin { .. } | SourceKind::RawStdout { .. } => return true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::{FromRawFd, IntoRawFd};

    /// Pipe ends as the reactor sees them: plain files it owns.
    fn to_file<T: IntoRawFd>(source: T) -> File {
        unsafe { File::from_raw_fd(source.into_raw_fd()) }
    }

    fn set_nonblocking(file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn nonblocking_pipe() -> (File, File) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        let read = unsafe { File::from_raw_fd(fds[0]) };
        let write = unsafe { File::from_raw_fd(fds[1]) };
        set_nonblocking(&read).expect("read nonblocking");
        set_nonblocking(&write).expect("write nonblocking");
        (read, write)
    }

    fn wake_pair() -> UnixStream {
        let (read, write) = UnixStream::pair().expect("wake pair");
        drop(read);
        write
    }

    /// Alternate flushes and reads until the queue is empty, returning every
    /// byte the pipe delivered.
    fn pump(queue: &RawOutQueue, read: &mut File, write: &mut File) -> Vec<u8> {
        let mut collected = Vec::new();
        let mut buffer = vec![0u8; 64 * 1024];
        for _ in 0..256 {
            queue.flush(write).expect("flush");
            loop {
                match read.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => collected.extend_from_slice(&buffer[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("read: {}", error),
                }
            }
            if !queue.has_pending() {
                break;
            }
        }
        assert!(!queue.has_pending(), "queue never drained");
        collected
    }

    #[test]
    fn raw_queue_bounds_the_frames_queued_for_one_plugin() {
        let queue = RawOutQueue::new(2, wake_pair());
        assert!(!queue.has_pending());
        queue.push(vec![1]).expect("first frame");
        queue.push(vec![2]).expect("second frame");
        assert_eq!(queue.push(vec![3]).err(), Some(RawFailure::WriterQueueFull));
        assert!(queue.has_pending());
    }

    #[test]
    fn raw_queue_keeps_a_partial_write_for_the_next_poll() {
        let (mut read, mut write) = nonblocking_pipe();
        let queue = RawOutQueue::new(1, wake_pair());
        // Bigger than any default pipe buffer, so one flush cannot finish it.
        let frame = vec![7u8; 256 * 1024];
        queue.push(frame.clone()).expect("push");

        // One pass writes only what the pipe accepts; the rest must survive.
        queue.flush(&mut write).expect("first flush");
        assert!(queue.has_pending(), "a 256 KiB frame cannot fit in a pipe");

        let delivered = pump(&queue, &mut read, &mut write);
        assert_eq!(delivered, frame);
    }

    /// One caller poll pass, returning the frames it delivered. `Closed` is
    /// ignored: the test asserts frames, not the end of the pipe.
    fn collect_frames(reactor: &Reactor, receiver: &Receiver<FrameEvent>) -> Vec<String> {
        reactor.poll_once(Duration::from_millis(10));
        let mut frames = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let FrameEvent::Frame(frame) = event {
                frames.push(frame.expect("frame"));
            }
        }
        frames
    }

    /// The supervisor loop owns the poll: nothing is delivered until the
    /// caller polls. A background reactor thread would deliver on its own.
    #[test]
    fn plugin_frames_arrive_only_when_the_caller_polls() {
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let reactor = Reactor::start().expect("reactor");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf '{\"jsonrpc\":\"2.0\",\"method\":\"svcd.ready\"}\\n'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("child");
        let (sender, receiver) = mpsc::sync_channel(4);
        reactor
            .register(
                "demo".into(),
                to_file(child.stdout.take().expect("stdout")),
                Some(to_file(child.stderr.take().expect("stderr"))),
                sender,
                None,
                None,
                4096,
                Framing::Lines,
            )
            .expect("register");
        // The child has exited, so its frame is already in the pipe. From
        // here on any delivery could only come from a thread this module owns.
        let _ = child.wait();
        assert!(
            receiver.try_recv().is_err(),
            "a background thread must not deliver plugin stdout"
        );

        let mut frame = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while frame.is_none() {
            assert!(
                Instant::now() < deadline,
                "poll_once did not deliver the plugin frame within 5 s"
            );
            reactor.poll_once(Duration::from_millis(50));
            if let Ok(FrameEvent::Frame(result)) = receiver.try_recv() {
                frame = Some(result.expect("frame"));
            }
        }
        assert_eq!(
            frame.as_deref(),
            Some("{\"jsonrpc\":\"2.0\",\"method\":\"svcd.ready\"}")
        );
    }

    /// A management request arrives while the supervisor loop waits inside the
    /// poll; the shared wake latch must end that wait, not the timeout.
    #[test]
    fn a_wake_notify_ends_a_blocking_poll_at_once() {
        use std::time::Instant;

        let reactor = Reactor::start().expect("reactor");
        let wake = Wake::new();
        wake.set_interrupt(reactor.interrupt_handle().expect("interrupt"));
        wake.notify();

        let started = Instant::now();
        reactor.poll_once(Duration::from_secs(30));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "notify must end the poll, not the timeout"
        );
    }

    /// The per-plugin frame queue is bounded. A full queue pauses that source
    /// instead of growing; the next caller poll hands over one more frame, so
    /// nothing is lost or reordered.
    #[test]
    fn a_full_frame_queue_pauses_that_source_until_the_caller_drains_it() {
        use std::process::{Command, Stdio};

        let reactor = Reactor::start().expect("reactor");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("printf 'one\\ntwo\\nthree\\n'")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("child");
        let (sender, receiver) = mpsc::sync_channel(1);
        reactor
            .register(
                "demo".into(),
                to_file(child.stdout.take().expect("stdout")),
                Some(to_file(child.stderr.take().expect("stderr"))),
                sender,
                None,
                None,
                64,
                Framing::Lines,
            )
            .expect("register");
        let _ = child.wait();

        assert_eq!(
            collect_frames(&reactor, &receiver),
            vec!["one".to_string()],
            "one pass must stop at the bounded queue instead of growing it"
        );
        assert_eq!(
            collect_frames(&reactor, &receiver),
            vec!["two".to_string()],
            "the paused source resumes only when the caller drains a slot"
        );
        assert_eq!(
            collect_frames(&reactor, &receiver),
            vec!["three".to_string()]
        );
    }


    #[test]
    fn raw_queue_preserves_frame_order_across_passes() {
        let (mut read, mut write) = nonblocking_pipe();
        let queue = RawOutQueue::new(4, wake_pair());
        queue.push(vec![1, 2, 3]).expect("first frame");
        queue.push(vec![4, 5, 6]).expect("second frame");
        let delivered = pump(&queue, &mut read, &mut write);
        assert_eq!(delivered, vec![1, 2, 3, 4, 5, 6]);
    }
}