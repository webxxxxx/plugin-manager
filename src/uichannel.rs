//! UI ↔ plugin realtime channel: process-local event bus, SSE and WebSocket.
//!
//! Plugin stdout stays a JSON-RPC NDJSON control channel. A plugin's
//! `notifications/*` frames are mirrored to UI subscribers; requests from the
//! UI still go through the same `tools/call` semantics as
//! `POST /plugin-ui/{id}/rpc`. The HTTP layer owns authentication and Origin
//! checks; this module owns the transports and the bounded fan-out.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};

use crate::channel;
use crate::rpc;
use crate::webui;

const MAX_SUBSCRIBERS: usize = 64;
const QUEUE: usize = 512;
const HISTORY_EVENTS: usize = 256;
const HISTORY_BYTES: usize = 2 * 1024 * 1024;
const MAX_MESSAGE: usize = 1024 * 1024;
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);
const WS_PING: Duration = Duration::from_secs(30);
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// One notification delivered to UI subscribers. `seq` is monotonic across the
/// process and is used as the SSE event id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    pub json: String,
}

pub struct EventBus {
    inner: Mutex<HashMap<String, PluginState>>,
    next_seq: AtomicU64,
    next_sub: AtomicU64,
}

#[derive(Default)]
struct PluginState {
    subscribers: Vec<(u64, SyncSender<Event>)>,
    history: VecDeque<Event>,
    history_bytes: usize,
}

pub struct Subscription {
    bus: Arc<EventBus>,
    plugin: String,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.unsubscribe(&self.plugin, self.id);
    }
}

#[derive(Debug)]
pub enum SubscribeError {
    TooMany,
}

/// What the SSE handler should say before the live stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Replay {
    /// The requested Last-Event-ID had already fallen out of the ring.
    pub reset: bool,
    /// How many historical events were queued for this subscription.
    pub replayed: usize,
}

static BUS: OnceLock<Arc<EventBus>> = OnceLock::new();

/// The daemon-wide bus. The daemon and HTTP listener are in one process.
pub fn bus() -> &'static Arc<EventBus> {
    BUS.get_or_init(|| Arc::new(EventBus::new()))
}

pub fn publish(plugin: &str, method: &str, params: &Value) {
    bus().publish(plugin, method, params);
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            inner: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
            next_sub: AtomicU64::new(0),
        }
    }

    pub fn subscribe(
        self: &Arc<Self>,
        plugin: &str,
    ) -> Result<(Receiver<Event>, Subscription), SubscribeError> {
        let (receiver, subscription, _) = self.subscribe_from(plugin, None)?;
        Ok((receiver, subscription))
    }

    /// Register a subscriber and, when last_event_id is present, queue the
    /// events after it before the live stream. History lookup and registration
    /// happen under one lock so a publish cannot slip between them.
    pub fn subscribe_from(
        self: &Arc<Self>,
        plugin: &str,
        last_event_id: Option<u64>,
    ) -> Result<(Receiver<Event>, Subscription, Replay), SubscribeError> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let id = self.next_sub.fetch_add(1, Ordering::SeqCst) + 1;
        let mut inner = self.lock();
        let state = inner.entry(plugin.to_string()).or_default();
        if state.subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(SubscribeError::TooMany);
        }

        let mut replay = Replay::default();
        if let Some(last) = last_event_id {
            if let (Some(oldest), Some(newest)) = (state.history.front(), state.history.back()) {
                replay.reset = last < oldest.seq.saturating_sub(1);
                if last < newest.seq {
                    for event in state.history.iter().filter(|event| event.seq > last) {
                        if sender.try_send(event.clone()).is_err() {
                            return Err(SubscribeError::TooMany);
                        }
                        replay.replayed += 1;
                    }
                }
            }
        }

        state.subscribers.push((id, sender));
        Ok((
            receiver,
            Subscription {
                bus: Arc::clone(self),
                plugin: plugin.to_string(),
                id,
            },
            replay,
        ))
    }

    #[allow(dead_code)]
    pub fn subscribe_with(
        self: &Arc<Self>,
        plugin: &str,
        sender: SyncSender<Event>,
    ) -> Result<Subscription, SubscribeError> {
        let id = self.next_sub.fetch_add(1, Ordering::SeqCst) + 1;
        let mut inner = self.lock();
        let state = inner.entry(plugin.to_string()).or_default();
        if state.subscribers.len() >= MAX_SUBSCRIBERS {
            return Err(SubscribeError::TooMany);
        }
        state.subscribers.push((id, sender));
        Ok(Subscription {
            bus: Arc::clone(self),
            plugin: plugin.to_string(),
            id,
        })
    }

    fn unsubscribe(&self, plugin: &str, id: u64) {
        let mut inner = self.lock();
        let mut remove = false;
        if let Some(state) = inner.get_mut(plugin) {
            state
                .subscribers
                .retain(|(subscriber, _)| *subscriber != id);
            remove = state.subscribers.is_empty() && state.history.is_empty();
        }
        if remove {
            inner.remove(plugin);
        }
    }

    pub fn publish(&self, plugin: &str, method: &str, params: &Value) {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let event = Event {
            seq,
            json: json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            })
            .to_string(),
        };
        let mut inner = self.lock();
        let state = inner.entry(plugin.to_string()).or_default();
        state.history_bytes = state.history_bytes.saturating_add(event.json.len());
        state.history.push_back(event.clone());
        while state.history.len() > HISTORY_EVENTS || state.history_bytes > HISTORY_BYTES {
            let Some(expired) = state.history.pop_front() else {
                break;
            };
            state.history_bytes = state.history_bytes.saturating_sub(expired.json.len());
        }
        state
            .subscribers
            .retain(|(_, sender)| match sender.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            });
    }

    #[allow(dead_code)]
    pub fn subscriber_count(&self, plugin: &str) -> usize {
        self.lock()
            .get(plugin)
            .map(|state| state.subscribers.len())
            .unwrap_or(0)
    }

    #[allow(dead_code)]
    pub fn history_len(&self, plugin: &str) -> usize {
        self.lock()
            .get(plugin)
            .map(|state| state.history.len())
            .unwrap_or(0)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, PluginState>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Write one SSE response and stream notifications until the client or the
/// subscription goes away. The caller has already authenticated the request.
pub fn serve_sse(
    stream: &mut TcpStream,
    receiver: Receiver<Event>,
    replay: Replay,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(SSE_HEARTBEAT + Duration::from_secs(5)))?;
    stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Connection: keep-alive\r\n\
          X-Accel-Buffering: no\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    )?;
    if replay.reset
        && write_chunk(
            stream,
            b"event: reset\ndata: {\"reason\":\"replay-unavailable\"}\n\n",
        )
        .is_err()
    {
        return Ok(());
    }
    loop {
        match receiver.recv_timeout(SSE_HEARTBEAT) {
            Ok(event) => {
                if write_chunk(stream, sse_event(&event).as_bytes()).is_err() {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if write_chunk(stream, b": ping\n\n").is_err() {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = stream.write_all(b"0\r\n\r\n");
                return Ok(());
            }
        }
    }
}

pub fn sse_event(event: &Event) -> String {
    format!(
        "id: {}\nevent: message\ndata: {}\n\n",
        event.seq, event.json
    )
}

fn write_chunk(stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    write!(stream, "{:x}\r\n", data.len())?;
    stream.write_all(data)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

/// Validate the RFC 6455 opening handshake and return the value for
/// `Sec-WebSocket-Accept`.
pub fn ws_accept(headers: &[(String, String)]) -> Result<String, &'static str> {
    let upgrade = header(headers, "upgrade").unwrap_or("");
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err("Upgrade must be websocket");
    }
    let connection = header(headers, "connection").unwrap_or("");
    if !connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
    {
        return Err("Connection must include Upgrade");
    }
    if header(headers, "sec-websocket-version") != Some("13") {
        return Err("Sec-WebSocket-Version must be 13");
    }
    let key = header(headers, "sec-websocket-key").ok_or("missing Sec-WebSocket-Key")?;
    let decoded = BASE64
        .decode(key.trim())
        .map_err(|_| "invalid Sec-WebSocket-Key")?;
    if decoded.len() != 16 {
        return Err("invalid Sec-WebSocket-Key");
    }
    Ok(accept_key(key.trim()))
}

pub fn accept_key(key: &str) -> String {
    BASE64.encode(sha1(format!("{}{}", key, WS_GUID).as_bytes()))
}

/// Read client frames from `stream` and write responses/notifications until
/// either side closes. `subscription` removes the bus subscriber on return.
pub fn serve_ws(
    stream: &mut TcpStream,
    config: &Path,
    plugin: &str,
    receiver: Receiver<Event>,
    subscription: Subscription,
) -> io::Result<()> {
    stream.set_read_timeout(None)?;
    let mut writer = stream.try_clone()?;
    let (out_tx, out_rx) = mpsc::sync_channel::<Outbound>(QUEUE);
    let stopped = Arc::new(AtomicBool::new(false));
    let writer_stopped = Arc::clone(&stopped);
    let writer = thread::Builder::new()
        .name("svcd-ws-writer".into())
        .spawn(move || write_loop(&mut writer, out_rx, &writer_stopped))?;

    let forward_stopped = Arc::clone(&stopped);
    let forward_tx = out_tx.clone();
    let forwarder = thread::Builder::new()
        .name("svcd-ws-events".into())
        .spawn(move || {
            for event in receiver {
                if forward_stopped.load(Ordering::SeqCst) {
                    break;
                }
                if forward_tx.send(Outbound::Text(event.json)).is_err() {
                    break;
                }
            }
        })?;

    let result = read_loop(stream, config, plugin, &out_tx);
    stopped.store(true, Ordering::SeqCst);
    drop(out_tx);
    drop(subscription);
    let _ = forwarder.join();
    let _ = writer.join();
    let _ = stream.shutdown(Shutdown::Both);
    result
}

#[derive(Debug)]
enum Outbound {
    Text(String),
    Pong(Vec<u8>),
    Close(u16, String),
}

fn write_loop(
    stream: &mut TcpStream,
    receiver: Receiver<Outbound>,
    stopped: &AtomicBool,
) -> io::Result<()> {
    let mut last_ping = Instant::now();
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Outbound::Text(text)) => {
                if write_frame(stream, 0x1, text.as_bytes()).is_err() {
                    break;
                }
            }
            Ok(Outbound::Pong(payload)) => {
                if write_frame(stream, 0xA, &payload).is_err() {
                    break;
                }
            }
            Ok(Outbound::Close(code, reason)) => {
                let mut payload = code.to_be_bytes().to_vec();
                payload.extend_from_slice(reason.as_bytes());
                let _ = write_frame(stream, 0x8, &payload);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                if last_ping.elapsed() >= WS_PING {
                    if write_frame(stream, 0x9, b"").is_err() {
                        break;
                    }
                    last_ping = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn read_loop(
    stream: &mut TcpStream,
    config: &Path,
    plugin: &str,
    out: &SyncSender<Outbound>,
) -> io::Result<()> {
    let mut fragment = Vec::new();
    let mut fragment_opcode: Option<u8> = None;
    loop {
        let frame = match read_frame(stream) {
            Ok(frame) => frame,
            Err(error) => {
                let (code, reason) = if error.to_string().contains("1 MiB") {
                    (1009, "message too large")
                } else {
                    (1002, "protocol error")
                };
                let _ = out.send(Outbound::Close(code, reason.into()));
                return Ok(());
            }
        };
        if frame.opcode >= 0x8 {
            if !frame.fin || frame.payload.len() > 125 {
                let _ = out.send(Outbound::Close(1002, "invalid control frame".into()));
                return Ok(());
            }
            match frame.opcode {
                0x8 => {
                    let code = if frame.payload.len() >= 2 {
                        u16::from_be_bytes([frame.payload[0], frame.payload[1]])
                    } else {
                        1000
                    };
                    let _ = out.send(Outbound::Close(code, String::new()));
                    return Ok(());
                }
                0x9 => {
                    if out.send(Outbound::Pong(frame.payload)).is_err() {
                        return Ok(());
                    }
                }
                0xA => {}
                _ => {
                    let _ = out.send(Outbound::Close(1002, "unknown control frame".into()));
                    return Ok(());
                }
            }
            continue;
        }

        match frame.opcode {
            0x1 | 0x2 => {
                if frame.opcode == 0x2 {
                    let _ = out.send(Outbound::Close(1003, "text frames only".into()));
                    return Ok(());
                }
                if fragment_opcode.is_some() {
                    let _ = out.send(Outbound::Close(1002, "interleaved data frame".into()));
                    return Ok(());
                }
                if !frame.fin {
                    fragment = frame.payload;
                    fragment_opcode = Some(0x1);
                } else if !handle_text(config, plugin, &frame.payload, out) {
                    return Ok(());
                }
            }
            0x0 => {
                if fragment_opcode.is_none() {
                    let _ = out.send(Outbound::Close(1002, "unexpected continuation".into()));
                    return Ok(());
                }
                fragment.extend_from_slice(&frame.payload);
                if fragment.len() > MAX_MESSAGE {
                    let _ = out.send(Outbound::Close(1009, "message too large".into()));
                    return Ok(());
                }
                if frame.fin {
                    let payload = std::mem::take(&mut fragment);
                    fragment_opcode = None;
                    if !handle_text(config, plugin, &payload, out) {
                        return Ok(());
                    }
                }
            }
            _ => {
                let _ = out.send(Outbound::Close(1002, "unknown data frame".into()));
                return Ok(());
            }
        }
    }
}

fn handle_text(config: &Path, plugin: &str, payload: &[u8], out: &SyncSender<Outbound>) -> bool {
    let response = match webui::bridge_request(plugin, payload) {
        webui::Bridge::Reject(response) => response,
        webui::Bridge::Forward {
            id,
            name,
            arguments,
        } => match channel::request(
            config,
            "registry.call",
            json!({ "name": name, "arguments": arguments }),
        ) {
            Ok(mut response) => {
                response.id = id;
                response
            }
            Err(cause) => rpc::err(id, -32603, &format!("svcd unreachable: {}", cause)),
        },
    };
    out.send(Outbound::Text(rpc::to_line(&response))).is_ok()
}

#[derive(Debug)]
struct Frame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Frame> {
    let mut header = [0u8; 2];
    reader.read_exact(&mut header)?;
    let fin = header[0] & 0x80 != 0;
    let opcode = header[0] & 0x0F;
    if header[0] & 0x70 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reserved frame bits are not supported",
        ));
    }
    let masked = header[1] & 0x80 != 0;
    if !masked {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "client frames must be masked",
        ));
    }
    let mut length = u64::from(header[1] & 0x7F);
    if length == 126 {
        let mut extended = [0u8; 2];
        reader.read_exact(&mut extended)?;
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0u8; 8];
        reader.read_exact(&mut extended)?;
        length = u64::from_be_bytes(extended);
    }
    let mut mask = [0u8; 4];
    reader.read_exact(&mut mask)?;
    if length > MAX_MESSAGE as u64 {
        // Consume enough of the oversized frame for the peer to finish writing
        // and receive our close frame instead of seeing a TCP reset.
        let drain = length.min(MAX_MESSAGE as u64 + 1024);
        let mut limited = reader.take(drain);
        io::copy(&mut limited, &mut io::sink())?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the 1 MiB limit",
        ));
    }
    if opcode >= 0x8 && (!fin || length > 125) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid control frame",
        ));
    }
    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload)?;
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok(Frame {
        fin,
        opcode,
        payload,
    })
}

fn write_frame<W: Write>(writer: &mut W, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | opcode);
    if payload.len() < 126 {
        header.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Minimal SHA-1 (RFC 3174) for the WebSocket accept key. The rest of the
/// project only needs SHA-256; adding a crate for one 20-byte digest would cost
/// binary size for no protocol benefit.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    let mut h = [
        0x6745_2301u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut digest = [0u8; 20];
    for (index, word) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::new();
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(out, "{:02x}", byte);
        }
        out
    }

    #[test]
    fn sha1_matches_the_rfc_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn websocket_accept_key_matches_rfc_6455() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn event_bus_fans_out_and_limits_subscribers() {
        let bus = Arc::new(EventBus::new());
        let (receiver, subscription) = bus.subscribe("demo").expect("subscribe");
        bus.publish("demo", "notifications/message", &json!({"data": "one"}));
        let event = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("event");
        assert!(event.json.contains("notifications/message"));
        assert!(event.json.contains("one"));
        drop(subscription);
        assert_eq!(bus.subscriber_count("demo"), 0);

        let mut held = Vec::new();
        for index in 0..MAX_SUBSCRIBERS {
            let (sender, receiver) = mpsc::sync_channel(1);
            drop(receiver);
            held.push(
                bus.subscribe_with("many", sender)
                    .unwrap_or_else(|_| panic!("subscriber {}", index)),
            );
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        assert!(matches!(
            bus.subscribe_with("many", sender),
            Err(SubscribeError::TooMany)
        ));
        drop(held);
    }

    #[test]
    fn event_bus_replays_after_last_event_id() {
        let bus = Arc::new(EventBus::new());
        bus.publish("replay", "notifications/message", &json!({"data": "one"}));
        bus.publish("replay", "notifications/message", &json!({"data": "two"}));
        bus.publish("replay", "notifications/message", &json!({"data": "three"}));
        let (receiver, subscription, replay) =
            bus.subscribe_from("replay", Some(1)).expect("subscribe");
        assert!(!replay.reset);
        assert_eq!(replay.replayed, 2);
        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("first replay");
        let second = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("second replay");
        assert_eq!((first.seq, second.seq), (2, 3));

        bus.publish("replay", "notifications/message", &json!({"data": "four"}));
        let live = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("live");
        assert_eq!(live.seq, 4);
        drop(subscription);
    }

    #[test]
    fn event_bus_marks_a_gap_when_last_event_id_falls_out() {
        let bus = Arc::new(EventBus::new());
        for index in 0..(HISTORY_EVENTS + 5) {
            bus.publish("gap", "notifications/message", &json!({"n": index}));
        }
        assert_eq!(bus.history_len("gap"), HISTORY_EVENTS);
        let (receiver, subscription, replay) =
            bus.subscribe_from("gap", Some(1)).expect("subscribe");
        assert!(replay.reset);
        assert_eq!(replay.replayed, HISTORY_EVENTS);
        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("replay");
        assert_eq!(first.seq, 6);
        drop(subscription);
    }

    #[test]
    fn masked_client_frames_round_trip() {
        let payload = b"{\"jsonrpc\":\"2.0\"}";
        let mask = [1u8, 2, 3, 4];
        let mut bytes = vec![0x81, 0x80 | payload.len() as u8];
        bytes.extend_from_slice(&mask);
        bytes.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        let mut cursor = Cursor::new(bytes);
        let frame = read_frame(&mut cursor).expect("frame");
        assert!(frame.fin);
        assert_eq!(frame.opcode, 0x1);
        assert_eq!(frame.payload, payload);

        let mut output = Vec::new();
        write_frame(&mut output, 0x1, payload).expect("write");
        assert_eq!(&output[..2], &[0x81, payload.len() as u8]);
        assert_eq!(&output[2..], payload);
    }

    #[test]
    fn sse_events_have_id_event_and_data() {
        let text = sse_event(&Event {
            seq: 7,
            json: "{\"jsonrpc\":\"2.0\"}".into(),
        });
        assert_eq!(
            text,
            "id: 7\nevent: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n"
        );
    }

    fn read_head(stream: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("response head");
            head.push(byte[0]);
        }
        String::from_utf8_lossy(&head).to_string()
    }

    fn client_send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
        let mask = [5u8, 6, 7, 8];
        let mut frame = vec![0x80 | opcode];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        stream.write_all(&frame).expect("client frame");
    }

    fn client_read(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).expect("server head");
        let opcode = header[0] & 0x0f;
        let mut length = u64::from(header[1] & 0x7f);
        if length == 126 {
            let mut extended = [0u8; 2];
            stream.read_exact(&mut extended).expect("server length");
            length = u64::from(u16::from_be_bytes(extended));
        }
        let mut payload = vec![0u8; length as usize];
        stream.read_exact(&mut payload).expect("server payload");
        (opcode, payload)
    }

    #[test]
    fn sse_transport_streams_events_over_tcp() {
        let bus = Arc::new(EventBus::new());
        let (receiver, subscription) = bus.subscribe("demo").expect("subscribe");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let address = listener.local_addr().expect("addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            serve_sse(&mut stream, receiver, Replay::default()).expect("serve sse");
        });

        let mut client = TcpStream::connect(address).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let head = read_head(&mut client);
        assert!(head.contains("text/event-stream"), "{head}");
        bus.publish("demo", "notifications/message", &json!({"data": "hello"}));
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while !seen.windows(5).any(|window| window == b"hello") {
            client.read_exact(&mut byte).expect("sse body");
            seen.push(byte[0]);
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("data:"),
            "{}",
            String::from_utf8_lossy(&seen)
        );
        drop(subscription);
        let _ = server.join();
    }

    #[test]
    fn sse_transport_writes_the_replay_reset_marker() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let address = listener.local_addr().expect("addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            serve_sse(
                &mut stream,
                receiver,
                Replay {
                    reset: true,
                    replayed: 0,
                },
            )
            .expect("serve sse");
        });

        let mut client = TcpStream::connect(address).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let head = read_head(&mut client);
        assert!(head.contains("text/event-stream"), "{head}");
        let mut body = Vec::new();
        let mut byte = [0u8; 1];
        while !body
            .windows(18)
            .any(|window| window == b"replay-unavailable")
        {
            client.read_exact(&mut byte).expect("reset marker");
            body.push(byte[0]);
        }
        drop(sender);
        let _ = server.join();
    }

    #[test]
    fn websocket_transport_writes_events_and_control_frames() {
        let bus = Arc::new(EventBus::new());
        let (receiver, subscription) = bus.subscribe("demo").expect("subscribe");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let address = listener.local_addr().expect("addr");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
                .expect("handshake");
            serve_ws(&mut stream, Path::new("."), "demo", receiver, subscription)
                .expect("serve ws");
        });

        let mut client = TcpStream::connect(address).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("timeout");
        let head = read_head(&mut client);
        assert!(head.contains("101"), "{head}");

        bus.publish("demo", "notifications/message", &json!({"data": "hello"}));
        let (opcode, payload) = client_read(&mut client);
        assert_eq!(opcode, 0x1);
        assert!(String::from_utf8_lossy(&payload).contains("hello"));

        client_send(&mut client, 0x9, b"ping");
        let (opcode, payload) = client_read(&mut client);
        assert_eq!(opcode, 0xA);
        assert_eq!(payload, b"ping");

        client_send(&mut client, 0x8, &[0x03, 0xE8]);
        let (opcode, _) = client_read(&mut client);
        assert_eq!(opcode, 0x8);
        let _ = server.join();
    }

    #[test]
    fn websocket_handshake_requires_upgrade_and_key() {
        let headers = vec![
            ("Upgrade".to_string(), "websocket".to_string()),
            ("Connection".to_string(), "keep-alive, Upgrade".to_string()),
            ("Sec-WebSocket-Version".to_string(), "13".to_string()),
            (
                "Sec-WebSocket-Key".to_string(),
                "dGhlIHNhbXBsZSBub25jZQ==".to_string(),
            ),
        ];
        assert!(ws_accept(&headers).is_ok());
        let mut bad = headers.clone();
        bad.retain(|(key, _)| key != "Sec-WebSocket-Key");
        assert!(ws_accept(&bad).is_err());
    }
}
