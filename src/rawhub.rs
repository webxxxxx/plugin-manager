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
//! One writer thread and one reader thread per plugin keep the HTTP reactor
//! non-blocking; the UI reactor polls each connection's bounded receiver.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

pub const HEADER: usize = 8;
const SUBSCRIBER_QUEUE: usize = 4;
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
    writer: SyncSender<Vec<u8>>,
    subscribers: Mutex<HashMap<u32, SyncSender<RawEvent>>>,
    inflight: Mutex<HashMap<u32, usize>>,
    closed: AtomicUsize,
}

impl RawPlugin {
    fn send(&self, stream: u32, payload: &[u8]) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) != 0 {
            return Err("raw plugin is closed".into());
        }
        if self
            .subscribers
            .lock()
            .map_err(|_| "raw subscriber lock poisoned")?
            .get(&stream)
            .is_none()
        {
            return Err("raw stream is not subscribed".into());
        }
        let mut frame = Vec::with_capacity(HEADER + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&stream.to_be_bytes());
        frame.extend_from_slice(payload);
        self.writer.try_send(frame).map_err(|error| match error {
            TrySendError::Full(_) => "raw plugin writer queue is full".to_string(),
            TrySendError::Disconnected(_) => "raw plugin writer is closed".to_string(),
        })
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
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                // A slow or dead UI must not grow daemon memory. Close its
                // stream and let the WebSocket side observe EOF.
                self.remove_subscriber(stream);
                hub.release_stream(self, stream);
                let _ = sender.try_send(RawEvent::Closed);
            }
        }
    }

    fn close(&self, hub: &RawHub) {
        self.closed.store(1, Ordering::SeqCst);
        if let Ok(mut subscribers) = self.subscribers.lock() {
            for sender in subscribers.values() {
                let _ = sender.try_send(RawEvent::Closed);
            }
            subscribers.clear();
        }
        if let Ok(streams) = self.inflight.lock() {
            for stream in streams.keys().copied().collect::<Vec<_>>() {
                hub.release_stream(self, stream);
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
    inflight_bytes: AtomicUsize,
    inflight_requests: AtomicUsize,
}

impl RawHub {
    pub fn new(
        max_frame: usize,
        max_inflight_bytes: usize,
        max_inflight_requests: usize,
        writer_queue: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            plugins: Mutex::new(HashMap::new()),
            next_stream: AtomicU32::new(1),
            max_frame,
            max_inflight_bytes,
            max_inflight_requests,
            writer_queue,
            inflight_bytes: AtomicUsize::new(0),
            inflight_requests: AtomicUsize::new(0),
        })
    }

    pub fn register(
        self: &Arc<Self>,
        id: String,
        mut stdin: ChildStdin,
        mut stdout: ChildStdout,
    ) -> Result<(), String> {
        let (writer_tx, writer_rx) = sync_channel::<Vec<u8>>(self.writer_queue);
        let plugin = Arc::new(RawPlugin {
            id: id.clone(),
            writer: writer_tx,
            subscribers: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            closed: AtomicUsize::new(0),
        });

        let writer_plugin = plugin.clone();
        let writer_hub = self.clone();
        thread::Builder::new()
            .name(format!("svcd-{}-raw-write", id))
            .stack_size(crate::threads::io_stack())
            .spawn(move || {
                while let Ok(frame) = writer_rx.recv() {
                    if stdin.write_all(&frame).is_err() {
                        break;
                    }
                    let _ = stdin.flush();
                }
                writer_plugin.close(&writer_hub);
            })
            .map_err(|error| format!("raw writer thread: {}", error))?;

        let reader_plugin = plugin.clone();
        let reader_hub = self.clone();
        let max_frame = self.max_frame;
        thread::Builder::new()
            .name(format!("svcd-{}-raw-read", id))
            .stack_size(crate::threads::io_stack())
            .spawn(move || {
                let mut header = [0u8; HEADER];
                loop {
                    if stdout.read_exact(&mut header).is_err() {
                        break;
                    }
                    let length =
                        u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
                    let stream = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
                    if length > max_frame {
                        break;
                    }
                    let mut payload = vec![0u8; length];
                    if stdout.read_exact(&mut payload).is_err() {
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

        let mut plugins = self
            .plugins
            .lock()
            .map_err(|_| "raw plugin lock poisoned".to_string())?;
        if let Some(old) = plugins.insert(id, plugin.clone()) {
            old.close(self);
        }
        Ok(())
    }

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

    fn reserve(&self, bytes: usize) -> bool {
        if bytes > self.max_inflight_bytes {
            return false;
        }
        let used = self.inflight_bytes.fetch_add(bytes, Ordering::SeqCst);
        if used.saturating_add(bytes) > self.max_inflight_bytes {
            self.inflight_bytes.fetch_sub(bytes, Ordering::SeqCst);
            return false;
        }
        let requests = self.inflight_requests.fetch_add(1, Ordering::SeqCst);
        if requests >= self.max_inflight_requests {
            self.inflight_requests.fetch_sub(1, Ordering::SeqCst);
            self.inflight_bytes.fetch_sub(bytes, Ordering::SeqCst);
            return false;
        }
        true
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
        if payload.len() > self.max_frame {
            return Err("raw frame is too large".into());
        }
        let plugin = &subscription.plugin;
        {
            let mut inflight = plugin
                .inflight
                .lock()
                .map_err(|_| "raw inflight lock poisoned".to_string())?;
            if inflight.contains_key(&subscription.stream) {
                return Err("raw stream already has an in-flight request".into());
            }
            if !self.reserve(payload.len()) {
                return Err("raw in-flight budget exceeded".into());
            }
            inflight.insert(subscription.stream, payload.len());
        }
        if let Err(error) = plugin.send(subscription.stream, payload) {
            self.release_stream(plugin, subscription.stream);
            return Err(error);
        }
        Ok(())
    }
}
