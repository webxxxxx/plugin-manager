#![cfg_attr(all(unix, not(test)), allow(dead_code))]
//! UI ↔ plugin realtime channel: process-local event bus, SSE and WebSocket.
//!
//! Plugin stdout stays a JSON-RPC NDJSON control channel. A plugin's
//! `notifications/*` frames are mirrored to UI subscribers; requests from the
//! UI still go through the same `tools/call` semantics as
//! `POST /plugin-ui/{id}/rpc`. The HTTP layer owns authentication and Origin
//! checks; this module owns the transports and the bounded fan-out.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};

const QUEUE: usize = 512;
const HISTORY_EVENTS: usize = 256;
const HISTORY_BYTES: usize = 2 * 1024 * 1024;
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
    total_subscribers: AtomicU64,
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

/// Forget a purged plugin's history and subscribers.
pub fn forget(plugin: &str) {
    bus().forget(plugin);
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            inner: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
            next_sub: AtomicU64::new(0),
            total_subscribers: AtomicU64::new(0),
        }
    }

    pub fn subscribe(
        self: &Arc<Self>,
        plugin: &str,
        limits: &crate::limits::Limits,
    ) -> Result<(Receiver<Event>, Subscription), SubscribeError> {
        let (receiver, subscription, _) = self.subscribe_from(plugin, None, limits)?;
        Ok((receiver, subscription))
    }

    /// Register a subscriber and, when last_event_id is present, queue the
    /// events after it before the live stream. History lookup and registration
    /// happen under one lock so a publish cannot slip between them.
    pub fn subscribe_from(
        self: &Arc<Self>,
        plugin: &str,
        last_event_id: Option<u64>,
        limits: &crate::limits::Limits,
    ) -> Result<(Receiver<Event>, Subscription, Replay), SubscribeError> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let id = self.next_sub.fetch_add(1, Ordering::SeqCst) + 1;
        self.reserve_subscriber(limits)?;
        let mut inner = self.lock();
        let state = inner.entry(plugin.to_string()).or_default();
        if state.subscribers.len() >= limits.max_subscribers_per_plugin {
            self.release_subscriber();
            return Err(SubscribeError::TooMany);
        }

        let mut replay = Replay::default();
        if let Some(last) = last_event_id {
            if let (Some(oldest), Some(newest)) = (state.history.front(), state.history.back()) {
                replay.reset = last < oldest.seq.saturating_sub(1);
                if last < newest.seq {
                    for event in state.history.iter().filter(|event| event.seq > last) {
                        if sender.try_send(event.clone()).is_err() {
                            self.release_subscriber();
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
        limits: &crate::limits::Limits,
    ) -> Result<Subscription, SubscribeError> {
        let id = self.next_sub.fetch_add(1, Ordering::SeqCst) + 1;
        self.reserve_subscriber(limits)?;
        let mut inner = self.lock();
        let state = inner.entry(plugin.to_string()).or_default();
        if state.subscribers.len() >= limits.max_subscribers_per_plugin {
            self.release_subscriber();
            return Err(SubscribeError::TooMany);
        }
        state.subscribers.push((id, sender));
        Ok(Subscription {
            bus: Arc::clone(self),
            plugin: plugin.to_string(),
            id,
        })
    }

    fn reserve_subscriber(&self, limits: &crate::limits::Limits) -> Result<(), SubscribeError> {
        let max = limits.max_subscriptions;
        let mut current = self.total_subscribers.load(Ordering::SeqCst);
        loop {
            if current as usize >= max {
                return Err(SubscribeError::TooMany);
            }
            match self.total_subscribers.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(actual) => current = actual,
            }
        }
    }

    fn release_subscriber(&self) {
        let mut current = self.total_subscribers.load(Ordering::SeqCst);
        loop {
            if current == 0 {
                return;
            }
            match self.total_subscribers.compare_exchange(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    fn unsubscribe(&self, plugin: &str, id: u64) {
        let mut inner = self.lock();
        let mut remove = false;
        let mut removed = false;
        if let Some(state) = inner.get_mut(plugin) {
            let before = state.subscribers.len();
            state
                .subscribers
                .retain(|(subscriber, _)| *subscriber != id);
            removed = state.subscribers.len() != before;
            remove = state.subscribers.is_empty() && state.history.is_empty();
        }
        if remove {
            inner.remove(plugin);
        }
        drop(inner);
        if removed {
            self.release_subscriber();
        }
    }

    /// Drop a purged plugin's history and subscribers.
    pub fn forget(&self, plugin: &str) {
        let removed = self
            .lock()
            .remove(plugin)
            .map(|state| state.subscribers.len())
            .unwrap_or(0);
        for _ in 0..removed {
            self.release_subscriber();
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

pub fn sse_event(event: &Event) -> String {
    format!(
        "id: {}\nevent: message\ndata: {}\n\n",
        event.seq, event.json
    )
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
        let limits = crate::limits::test_default();
        let (receiver, subscription) = bus
            .subscribe("demo", &crate::limits::test_default())
            .expect("subscribe");
        bus.publish("demo", "notifications/message", &json!({"data": "one"}));
        let event = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("event");
        assert!(event.json.contains("notifications/message"));
        assert!(event.json.contains("one"));
        drop(subscription);
        assert_eq!(bus.subscriber_count("demo"), 0);

        let mut held = Vec::new();
        for index in 0..limits.max_subscribers_per_plugin {
            let (sender, receiver) = mpsc::sync_channel(1);
            drop(receiver);
            held.push(
                bus.subscribe_with("many", sender, &limits)
                    .unwrap_or_else(|_| panic!("subscriber {}", index)),
            );
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        assert!(matches!(
            bus.subscribe_with("many", sender, &limits),
            Err(SubscribeError::TooMany)
        ));
        drop(held);
    }

    #[test]
    fn event_bus_forgets_a_purged_plugin() {
        let bus = Arc::new(EventBus::new());
        let (_receiver, subscription) = bus
            .subscribe("demo", &crate::limits::test_default())
            .expect("subscribe");
        bus.publish("demo", "notifications/message", &json!({"data": "one"}));
        assert!(bus.history_len("demo") > 0);

        bus.forget("demo");

        assert_eq!(bus.subscriber_count("demo"), 0);
        assert_eq!(bus.history_len("demo"), 0);
        drop(subscription);
    }

    #[test]
    fn event_bus_replays_after_last_event_id() {
        let bus = Arc::new(EventBus::new());
        bus.publish("replay", "notifications/message", &json!({"data": "one"}));
        bus.publish("replay", "notifications/message", &json!({"data": "two"}));
        bus.publish("replay", "notifications/message", &json!({"data": "three"}));
        let (receiver, subscription, replay) = bus
            .subscribe_from("replay", Some(1), &crate::limits::test_default())
            .expect("subscribe");
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
        let (receiver, subscription, replay) = bus
            .subscribe_from("gap", Some(1), &crate::limits::test_default())
            .expect("subscribe");
        assert!(replay.reset);
        assert_eq!(replay.replayed, HISTORY_EVENTS);
        let first = receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("replay");
        assert_eq!(first.seq, 6);
        drop(subscription);
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
