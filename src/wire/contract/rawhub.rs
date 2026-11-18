//! Raw plugin transport (protocol 3).
#![allow(dead_code)]
//!
//! A protocol-3 plugin exchanges length-prefixed raw frames on stdin/stdout:
//!
//! ```text
//!   u32 payload_len (big endian)
//!   u32 stream_id  (big endian)
//!   payload
//! ```
//!
//! The hub owns the stream table, the subscriber channels and the global
//! in-flight budget. The I/O itself belongs to [`crate::wire::contract::reactor`] on Unix: a
//! raw plugin costs no extra thread there. When the reactor is unavailable
//! (and on Windows, where anonymous child pipes are not pollable) the hub
//! falls back to one writer thread and one reader thread per plugin.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

#[cfg(unix)]
use std::sync::Weak;

use crate::platform::log::LogSink;
use crate::wire::contract::stdio::{self, ReadEnd, Streams, WriteEnd};
use crate::wire::message::ipc::{RawFailure, RawStopSink};

pub const HEADER: usize = 8;
const SUBSCRIBER_QUEUE: usize = 4;

impl RawStopSink for RawHub {
    fn note_transport_failure(&self, failure: RawFailure, plugin: &str) {
        RawHub::note_transport_failure(self, failure, plugin);
    }

    fn unregister_generation(&self, id: &str, generation: u64) {
        RawHub::unregister_generation(self, id, generation);
    }

    fn deliver(&self, id: &str, stream: u32, payload: Vec<u8>) {
        RawHub::deliver(self, id, stream, payload);
    }
}

const FAILURES: usize = RawFailure::ALL.len();

/// Per-daemon failure counters. Each reason logs on powers of two, so a flood
/// of rejections costs O(log n) log lines while still reporting the totals.
pub struct RawStats {
    counts: [AtomicU64; FAILURES],
}

impl Default for RawStats {
    fn default() -> Self {
        Self {
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl RawStats {
    pub fn record(&self, failure: RawFailure, plugin: &str, stream: u32, detail: &str) -> u64 {
        let total = self.counts[failure.index()].fetch_add(1, Ordering::Relaxed) + 1;
        if total.is_power_of_two() {
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(" ({})", detail)
            };
            crate::platform::log::svcd(&format!(
                "svcd: raw {}: plugin={} stream={} total={}{}",
                failure.as_str(),
                plugin,
                stream,
                total,
                detail
            ));
        }
        total
    }

    pub fn count(&self, failure: RawFailure) -> u64 {
        self.counts[failure.index()].load(Ordering::Relaxed)
    }

    /// One line with every non-zero counter, for shutdown and diagnostics.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for failure in RawFailure::ALL {
            let count = self.count(failure);
            if count > 0 {
                parts.push(format!("{}={}", failure.as_str(), count));
            }
        }
        if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" ")
        }
    }
}

#[derive(Debug)]
pub enum RawEvent {
    Frame(Vec<u8>),
    Closed,
}

pub struct RawSubscription {
    hub: Arc<RawHub>,
    plugin: Arc<RawPlugin>,
    stream: u32,
    receiver: Receiver<RawEvent>,
}

impl RawSubscription {
    pub fn stream(&self) -> u32 {
        self.stream
    }

    pub fn plugin(&self) -> &str {
        &self.plugin.id
    }

    pub fn try_recv(&self) -> Result<RawEvent, std::sync::mpsc::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn send(&self, payload: &[u8]) -> Result<(), String> {
        self.hub.send(self, payload)
    }
}

impl Drop for RawSubscription {
    fn drop(&mut self) {
        self.plugin.remove_subscriber(self.stream);
        self.hub.release_stream(&self.plugin, self.stream);
    }
}

struct RawPlugin {
    id: String,
    /// Monotonic identity of this transport instance. The reactor holds only a
    /// weak hub handle, so it uses this to avoid unregistering a replacement
    /// that was installed while its own pipes were closing.
    generation: u64,
    subscribers: Mutex<HashMap<u32, SyncSender<RawEvent>>>,
    inflight: Mutex<HashMap<u32, usize>>,
    closed: AtomicBool,
    /// Reactor-owned stdin queue once the reactor has adopted this plugin.
    #[cfg(unix)]
    out: OnceLock<Arc<crate::wire::contract::reactor::RawOutQueue>>,
    /// Fallback writer-thread queue.
    writer: OnceLock<SyncSender<Vec<u8>>>,
}

impl RawPlugin {
    fn new(id: String, generation: u64) -> Self {
        Self {
            id,
            generation,
            subscribers: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            #[cfg(unix)]
            out: OnceLock::new(),
            writer: OnceLock::new(),
        }
    }

    fn remove_subscriber(&self, stream: u32) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.remove(&stream);
        }
    }

    fn publish(&self, stream: u32, payload: Vec<u8>, hub: &RawHub) {
        let sender = self
            .subscribers
            .lock()
            .ok()
            .and_then(|subscribers| subscribers.get(&stream).cloned());
        let Some(sender) = sender else {
            return;
        };
        match sender.try_send(RawEvent::Frame(payload)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // A slow or dead UI must not grow daemon memory. Close its
                // stream and let the WebSocket side observe EOF.
                hub.stats
                    .record(RawFailure::SubscriberFull, &self.id, stream, "");
                self.remove_subscriber(stream);
                hub.release_stream(self, stream);
                let _ = sender.try_send(RawEvent::Closed);
            }
            Err(TrySendError::Disconnected(_)) => {
                hub.stats
                    .record(RawFailure::SubscriberClosed, &self.id, stream, "");
                self.remove_subscriber(stream);
                hub.release_stream(self, stream);
            }
        }
    }

    fn close(&self, hub: &RawHub) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut subscribers) = self.subscribers.lock() {
            for sender in subscribers.values() {
                let _ = sender.try_send(RawEvent::Closed);
            }
            subscribers.clear();
        }
        if let Ok(mut inflight) = self.inflight.lock() {
            let bytes: Vec<usize> = inflight.drain().map(|(_, bytes)| bytes).collect();
            drop(inflight);
            for bytes in bytes {
                hub.release(bytes);
            }
        }
    }
}

pub struct RawHub {
    plugins: Mutex<HashMap<String, Arc<RawPlugin>>>,
    next_stream: AtomicU32,
    max_frame: usize,
    max_inflight_bytes: usize,
    max_inflight_requests: usize,
    writer_queue: usize,
    limits: Arc<crate::platform::limits::Limits>,
    inflight_bytes: AtomicUsize,
    inflight_requests: AtomicUsize,
    next_generation: AtomicU64,
    /// Weak so the daemon's `Arc<Reactor>` stays the single owner.
    #[cfg(unix)]
    reactor: OnceLock<Weak<crate::wire::contract::reactor::Reactor>>,
    pub stats: RawStats,
}

impl RawHub {
    pub fn new(limits: Arc<crate::platform::limits::Limits>) -> Arc<Self> {
        Arc::new(Self {
            plugins: Mutex::new(HashMap::new()),
            next_stream: AtomicU32::new(1),
            max_frame: limits.ws_raw_max_bytes,
            max_inflight_bytes: limits.ws_inflight_bytes,
            max_inflight_requests: limits.ws_inflight_requests,
            writer_queue: limits.ws_raw_queue.max(1),
            limits,
            inflight_bytes: AtomicUsize::new(0),
            inflight_requests: AtomicUsize::new(0),
            next_generation: AtomicU64::new(1),
            #[cfg(unix)]
            reactor: OnceLock::new(),
            stats: RawStats::default(),
        })
    }

    /// Adopt the plugin-stdio reactor. Plugins registered after this call run
    /// without per-plugin threads.
    #[cfg(unix)]
    pub fn set_reactor(&self, reactor: &Arc<crate::wire::contract::reactor::Reactor>) {
        let _ = self.reactor.set(Arc::downgrade(reactor));
    }

    #[cfg(unix)]
    fn reactor(&self) -> Option<Arc<crate::wire::contract::reactor::Reactor>> {
        self.reactor.get().and_then(|reactor| reactor.upgrade())
    }

    /// Adopt a protocol-3 plugin's pipes.
    ///
    /// The pipe layer opened them and chose the handle types; the hub owns the
    /// stream table either way. On Unix the poll set drives the I/O (the write
    /// end becomes its bounded frame queue); everywhere else one writer, one
    /// reader and one stderr thread do.
    pub fn register(
        self: &Arc<Self>,
        id: String,
        streams: Streams,
        log: Option<Arc<Mutex<LogSink>>>,
    ) -> Result<(), String> {
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let Streams {
            stdin,
            stdout,
            stderr,
        } = streams;
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            return Err("raw stdio is not piped".into());
        };
        #[cfg(unix)]
        if let Some(reactor) = self.reactor() {
            let stdin =
                stdio::poll_write(stdin).map_err(|error| format!("raw reactor: {}", error))?;
            let stdout =
                stdio::poll_read(stdout).map_err(|error| format!("raw reactor: {}", error))?;
            let stderr = stderr
                .map(stdio::poll_read)
                .transpose()
                .map_err(|error| format!("raw reactor: {}", error))?;
            let queue = reactor
                .register_raw(
                    id.clone(),
                    stdin,
                    stdout,
                    stderr,
                    log,
                    Arc::downgrade(self) as Weak<dyn RawStopSink>,
                    generation,
                    self.max_frame,
                    self.writer_queue,
                )
                .map_err(|error| format!("raw reactor: {}", error))?;
            let plugin = Arc::new(RawPlugin::new(id.clone(), generation));
            let _ = plugin.out.set(queue);
            return self.install(id, plugin);
        }
        self.register_threads(id, generation, stdin, stdout, stderr, log)
    }

    /// Fallback when there is no reactor (reactor startup failed, or a
    /// non-Unix build): one writer thread, one reader thread and one stderr
    /// thread per plugin. Protocol-3 plugins still start this way on Windows,
    /// but the `/raw` UI route answers 501 there, so nothing can subscribe.
    fn register_threads(
        self: &Arc<Self>,
        id: String,
        generation: u64,
        mut stdin: WriteEnd,
        mut stdout: ReadEnd,
        stderr: Option<ReadEnd>,
        log: Option<Arc<Mutex<LogSink>>>,
    ) -> Result<(), String> {
        let (writer_tx, writer_rx) = sync_channel::<Vec<u8>>(self.writer_queue);
        let plugin = Arc::new(RawPlugin::new(id.clone(), generation));
        let _ = plugin.writer.set(writer_tx);

        let writer_plugin = plugin.clone();
        let writer_hub = self.clone();
        let writer_id = id.clone();
        thread::Builder::new()
            .name(format!("svcd-{}-raw-write", id))
            .stack_size(crate::platform::host::threads::io_stack(&self.limits))
            .spawn(move || {
                while let Ok(frame) = writer_rx.recv() {
                    if stdin.write_all(&frame).is_err() {
                        writer_hub.stats.record(
                            RawFailure::PluginWriteError,
                            &writer_id,
                            0,
                            "stdin write",
                        );
                        break;
                    }
                    let _ = stdin.flush();
                }
                writer_plugin.close(&writer_hub);
            })
            .map_err(|error| format!("raw writer thread: {}", error))?;

        let reader_plugin = plugin.clone();
        let reader_hub = self.clone();
        let reader_id = id.clone();
        let max_frame = self.max_frame;
        thread::Builder::new()
            .name(format!("svcd-{}-raw-read", id))
            .stack_size(crate::platform::host::threads::io_stack(&self.limits))
            .spawn(move || {
                let mut header = [0u8; HEADER];
                loop {
                    if let Err(error) = stdout.read_exact(&mut header) {
                        // UnexpectedEof is how a cleanly closed plugin stdout
                        // surfaces; anything else is an I/O failure.
                        let failure = if error.kind() == std::io::ErrorKind::UnexpectedEof {
                            RawFailure::PluginEof
                        } else {
                            RawFailure::PluginReadError
                        };
                        reader_hub
                            .stats
                            .record(failure, &reader_id, 0, "stdout header");
                        break;
                    }
                    let length =
                        u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
                    let stream = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                    if length > max_frame {
                        reader_hub.stats.record(
                            RawFailure::FrameTooLarge,
                            &reader_id,
                            stream,
                            "plugin frame",
                        );
                        break;
                    }
                    let mut payload = vec![0u8; length];
                    if let Err(error) = stdout.read_exact(&mut payload) {
                        let failure = if error.kind() == std::io::ErrorKind::UnexpectedEof {
                            RawFailure::PluginEof
                        } else {
                            RawFailure::PluginReadError
                        };
                        reader_hub
                            .stats
                            .record(failure, &reader_id, stream, "stdout payload");
                        break;
                    }
                    let removed = reader_plugin
                        .inflight
                        .lock()
                        .ok()
                        .and_then(|mut inflight| inflight.remove(&stream));
                    if let Some(length) = removed {
                        reader_hub.release(length);
                    }
                    reader_plugin.publish(stream, payload, &reader_hub);
                }
                reader_plugin.close(&reader_hub);
            })
            .map_err(|error| format!("raw reader thread: {}", error))?;

        if let Some(stderr) = stderr {
            stdio::log_lines(format!("svcd-{}-stderr", id), stderr, log, &self.limits)
                .map_err(|error| format!("raw stderr thread: {}", error))?;
        }
        self.install(id, plugin)
    }

    fn install(&self, id: String, plugin: Arc<RawPlugin>) -> Result<(), String> {
        let mut plugins = self
            .plugins
            .lock()
            .map_err(|_| "raw plugin lock poisoned".to_string())?;
        if let Some(old) = plugins.insert(id, plugin) {
            self.stats
                .record(RawFailure::PluginReplaced, &old.id, 0, "");
            old.close(self);
        }
        Ok(())
    }

    /// Record a transport-level failure that has no stream of its own, as the
    /// reactor reports when a plugin's pipes close.
    pub fn note_transport_failure(&self, failure: RawFailure, plugin: &str) {
        self.stats.record(failure, plugin, 0, "reactor");
    }

    /// Drop a plugin's transport. The service lifecycle calls this when it
    /// stops a plugin, so it removes whatever instance is installed.
    pub fn unregister(&self, id: &str) {
        let plugin = self
            .plugins
            .lock()
            .ok()
            .and_then(|mut plugins| plugins.remove(id));
        if let Some(plugin) = plugin {
            plugin.close(self);
        }
    }

    /// Drop a plugin's transport only when `generation` is still the installed
    /// instance. The reactor reports a dead pipe with the generation it
    /// registered; a restart may already have replaced it.
    pub fn unregister_generation(&self, id: &str, generation: u64) {
        let plugin = self.plugins.lock().ok().and_then(|mut plugins| {
            if plugins.get(id).map(|plugin| plugin.generation) == Some(generation) {
                plugins.remove(id)
            } else {
                None
            }
        });
        if let Some(plugin) = plugin {
            plugin.close(self);
        }
    }

    /// A complete frame from the plugin's stdout, addressed to `stream`.
    pub fn deliver(&self, id: &str, stream: u32, payload: Vec<u8>) {
        let plugin = self
            .plugins
            .lock()
            .ok()
            .and_then(|plugins| plugins.get(id).cloned());
        let Some(plugin) = plugin else {
            return;
        };
        // Release the in-flight reservation before publishing so the client can
        // send its next request as soon as it sees this frame.
        self.release_stream(&plugin, stream);
        plugin.publish(stream, payload, self);
    }

    fn reserve(&self, bytes: usize) -> Result<(), RawFailure> {
        if bytes > self.max_inflight_bytes {
            return Err(RawFailure::BudgetBytes);
        }
        let used = self.inflight_bytes.fetch_add(bytes, Ordering::SeqCst);
        if used.saturating_add(bytes) > self.max_inflight_bytes {
            self.inflight_bytes.fetch_sub(bytes, Ordering::SeqCst);
            return Err(RawFailure::BudgetBytes);
        }
        let requests = self.inflight_requests.fetch_add(1, Ordering::SeqCst);
        if requests >= self.max_inflight_requests {
            self.inflight_requests.fetch_sub(1, Ordering::SeqCst);
            self.inflight_bytes.fetch_sub(bytes, Ordering::SeqCst);
            return Err(RawFailure::BudgetRequests);
        }
        Ok(())
    }

    fn release(&self, bytes: usize) {
        self.inflight_bytes.fetch_sub(bytes, Ordering::SeqCst);
        self.inflight_requests.fetch_sub(1, Ordering::SeqCst);
    }

    fn release_stream(&self, plugin: &RawPlugin, stream: u32) {
        let bytes = plugin
            .inflight
            .lock()
            .ok()
            .and_then(|mut inflight| inflight.remove(&stream));
        if let Some(bytes) = bytes {
            self.release(bytes);
        }
    }

    pub fn subscribe(self: &Arc<Self>, id: &str) -> Result<RawSubscription, String> {
        let plugin = self
            .plugins
            .lock()
            .map_err(|_| "raw plugin lock poisoned".to_string())?
            .get(id)
            .cloned()
            .ok_or_else(|| format!("plugin {} has no raw transport", id))?;
        let stream = self.next_stream.fetch_add(1, Ordering::Relaxed);
        if stream == 0 {
            return Err("raw stream id exhausted".into());
        }
        let (sender, receiver) = sync_channel(SUBSCRIBER_QUEUE);
        plugin
            .subscribers
            .lock()
            .map_err(|_| "raw subscriber lock poisoned".to_string())?
            .insert(stream, sender);
        Ok(RawSubscription {
            hub: self.clone(),
            plugin,
            stream,
            receiver,
        })
    }

    pub fn send(&self, subscription: &RawSubscription, payload: &[u8]) -> Result<(), String> {
        let plugin = &subscription.plugin;
        let stream = subscription.stream;
        if payload.len() > self.max_frame {
            self.stats
                .record(RawFailure::FrameTooLarge, &plugin.id, stream, "");
            return Err(RawFailure::FrameTooLarge.message().to_string());
        }
        if plugin.closed.load(Ordering::SeqCst) {
            self.stats
                .record(RawFailure::WriterClosed, &plugin.id, stream, "closed");
            return Err(RawFailure::WriterClosed.message().to_string());
        }
        let subscribed = plugin
            .subscribers
            .lock()
            .map(|subscribers| subscribers.contains_key(&stream))
            .unwrap_or(false);
        if !subscribed {
            self.stats
                .record(RawFailure::StreamNotSubscribed, &plugin.id, stream, "");
            return Err(RawFailure::StreamNotSubscribed.message().to_string());
        }
        let mut inflight = plugin
            .inflight
            .lock()
            .map_err(|_| "raw inflight lock poisoned".to_string())?;
        if inflight.contains_key(&stream) {
            self.stats
                .record(RawFailure::StreamConflict, &plugin.id, stream, "");
            return Err(RawFailure::StreamConflict.message().to_string());
        }
        if let Err(failure) = self.reserve(payload.len()) {
            self.stats.record(failure, &plugin.id, stream, "");
            return Err(failure.message().to_string());
        }
        inflight.insert(stream, payload.len());
        drop(inflight);
        let mut frame = Vec::with_capacity(HEADER + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&stream.to_be_bytes());
        frame.extend_from_slice(payload);
        if let Err(failure) = self.write_frame(plugin, frame) {
            self.stats.record(failure, &plugin.id, stream, "");
            self.release_stream(plugin, stream);
            return Err(failure.message().to_string());
        }
        Ok(())
    }

    fn write_frame(&self, plugin: &RawPlugin, frame: Vec<u8>) -> Result<(), RawFailure> {
        #[cfg(unix)]
        if let Some(queue) = plugin.out.get() {
            return queue.push(frame);
        }
        match plugin.writer.get() {
            Some(writer) => writer.try_send(frame).map_err(|error| match error {
                TrySendError::Full(_) => RawFailure::WriterQueueFull,
                TrySendError::Disconnected(_) => RawFailure::WriterClosed,
            }),
            None => Err(RawFailure::WriterClosed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_count_each_failure_and_summarise() {
        let stats = RawStats::default();
        assert_eq!(stats.count(RawFailure::WriterQueueFull), 0);
        stats.record(RawFailure::WriterQueueFull, "p01", 7, "queue");
        stats.record(RawFailure::WriterQueueFull, "p01", 8, "");
        stats.record(RawFailure::BudgetBytes, "p02", 9, "");
        assert_eq!(stats.count(RawFailure::WriterQueueFull), 2);
        let summary = stats.summary();
        assert!(summary.contains("writer_queue_full=2"), "{}", summary);
        assert!(summary.contains("budget_bytes=1"), "{}", summary);
        assert!(!summary.contains("plugin_eof"), "{}", summary);
        assert_eq!(RawStats::default().summary(), "none");
    }

    #[test]
    fn every_failure_has_its_own_counter_and_name() {
        let mut names: Vec<&str> = RawFailure::ALL.iter().map(|kind| kind.as_str()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "duplicate counter name");

        // The counter table must cover every variant exactly once.
        let mut indexes: Vec<usize> = RawFailure::ALL.iter().map(|kind| kind.index()).collect();
        indexes.sort_unstable();
        indexes.dedup();
        assert_eq!(indexes.len(), total, "two failures share a counter slot");
        assert_eq!(indexes, (0..total).collect::<Vec<_>>());
    }

    #[test]
    fn pending_payload_counters_are_balanced() {
        let mut settings = crate::platform::settings::Settings::default();
        settings.ws_inflight_bytes = Some(64 * 1024);
        settings.ws_inflight_requests = Some(4);
        let hub = RawHub::new(Arc::new(crate::platform::limits::derive(&settings)));
        assert!(hub.reserve(1024).is_ok());
        assert_eq!(hub.inflight_bytes.load(Ordering::SeqCst), 1024);
        hub.release(1024);
        assert_eq!(hub.inflight_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(hub.inflight_requests.load(Ordering::SeqCst), 0);

        // One frame larger than the whole budget never reserves.
        assert_eq!(hub.reserve(65 * 1024).err(), Some(RawFailure::BudgetBytes));
        assert_eq!(hub.inflight_bytes.load(Ordering::SeqCst), 0);

        // The request cap holds even when bytes are available.
        assert!(hub.reserve(1).is_ok());
        assert!(hub.reserve(1).is_ok());
        assert!(hub.reserve(1).is_ok());
        assert!(hub.reserve(1).is_ok());
        assert_eq!(hub.reserve(1).err(), Some(RawFailure::BudgetRequests));
        for _ in 0..4 {
            hub.release(1);
        }
        assert_eq!(hub.inflight_requests.load(Ordering::SeqCst), 0);
    }
}
