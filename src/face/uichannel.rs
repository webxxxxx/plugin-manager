#![cfg_attr(not(feature = "http"), allow(dead_code))]
#![cfg_attr(all(unix, not(test)), allow(dead_code))]
//! UI ↔ plugin realtime channel: injected event bus, SSE and WebSocket.
//!
//! Plugin stdout stays a JSON-RPC NDJSON control channel. A plugin's
//! notifications/* frames are mirrored to UI subscribers; requests from the
//! UI still go through the same tools/call semantics as
//! POST /plugin-ui/{id}/rpc. The HTTP layer owns authentication and Origin
//! checks; this module owns the transports and the bounded fan-out.

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

/// Rows kept in the one ring. The byte cap below is the real bound; this one
/// keeps a burst of tiny events from pinning the ring at its byte ceiling.
const HISTORY_EVENTS: usize = 1024;
/// The one bounded buffer behind every plugin's UI stream. Subscribers hold a
/// cursor, not a queue, so memory does not grow with subscriber or plugin
/// count: a slow subscriber falls behind the ring and is resynchronised with a
/// reset marker instead of a disconnect.
pub const HISTORY_BYTES: usize = crate::platform::limits::UI_RING_BYTES as usize;

/// One notification delivered to UI subscribers. seq is monotonic across the
/// process and is used as the SSE event id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    /// Which plugin published it. One ring holds every plugin's events, so a
    /// subscriber skips the rows of the others.
    pub plugin: String,
    pub json: String,
}

/// What a reactor installs so a published event does not wait for the loop's
/// poll cadence. Called from the publishing thread, never from the loop.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct EventBus {
    inner: Mutex<Ring>,
    next_seq: AtomicU64,
    next_sub: AtomicU64,
    /// Installed by the host's connection driver. Absent on a driver that
    /// cannot be woken (the thread-per-connection fallback), where the
    /// handler's own poll cadence delivers the event instead.
    wake: Mutex<Option<Wake>>,
}

struct Ring {
    /// Seq-ordered, possibly with holes where a purged plugin's rows were
    /// removed; readers scan by seq, never by position.
    events: VecDeque<Event>,
    bytes: usize,
    subscribers: Vec<Subscriber>,
}

struct Subscriber {
    id: u64,
    plugin: String,
}

impl Ring {
    fn front_seq(&self) -> Option<u64> {
        self.events.front().map(|event| event.seq)
    }

    fn latest_seq(&self) -> u64 {
        self.events.back().map(|event| event.seq).unwrap_or(0)
    }

    fn trim(&mut self) {
        while self.events.len() > HISTORY_EVENTS || self.bytes > HISTORY_BYTES {
            let Some(expired) = self.events.pop_front() else {
                break;
            };
            self.bytes = self
                .bytes
                .saturating_sub(expired.json.len() + expired.plugin.len());
        }
    }
}

pub struct Subscription {
    bus: Arc<EventBus>,
    plugin: String,
    id: u64,
    /// Seq of the last event handed to the handler. A live subscription starts
    /// at the ring's latest seq.
    cursor: u64,
}

/// What one poll found for this subscriber.
#[derive(Debug)]
pub enum Polled {
    Event(Event),
    /// The next expected seq had already fallen out of the ring. The cursor
    /// jumped to the front; the handler writes a reset marker and resumes.
    Reset,
    Empty,
}

impl Subscription {
    /// Take the next event this subscriber may not have seen. A handler calls
    /// this from its writable path; there is no queue to drain, so a subscriber
    /// that stops reading costs nothing but its cursor.
    pub fn poll(&mut self) -> Polled {
        let inner = self.bus.lock();
        let Some(front) = inner.front_seq() else {
            return Polled::Empty;
        };
        if self.cursor.saturating_add(1) < front {
            self.cursor = front.saturating_sub(1);
            return Polled::Reset;
        }
        let start = self.cursor.saturating_add(1).max(front);
        let found = inner
            .events
            .iter()
            .find(|event| event.seq >= start && event.plugin == self.plugin)
            .cloned();
        match found {
            Some(event) => {
                self.cursor = event.seq;
                Polled::Event(event)
            }
            None => Polled::Empty,
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.unsubscribe(self.id);
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
    /// How many ring rows the subscription will deliver before live events.
    pub replayed: usize,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            inner: Mutex::new(Ring {
                events: VecDeque::new(),
                bytes: 0,
                subscribers: Vec::new(),
            }),
            next_seq: AtomicU64::new(0),
            next_sub: AtomicU64::new(0),
            wake: Mutex::new(None),
        }
    }

    pub fn subscribe(
        self: &Arc<Self>,
        plugin: &str,
        limits: &crate::platform::limits::Limits,
    ) -> Result<Subscription, SubscribeError> {
        let (subscription, _) = self.subscribe_from(plugin, None, limits)?;
        Ok(subscription)
    }

    /// Register a subscriber and, when last_event_id is present, point it at
    /// the events after it. Registration and the cursor both happen under the
    /// ring lock, so a publish cannot slip between them.
    pub fn subscribe_from(
        self: &Arc<Self>,
        plugin: &str,
        last_event_id: Option<u64>,
        limits: &crate::platform::limits::Limits,
    ) -> Result<(Subscription, Replay), SubscribeError> {
        let mut inner = self.lock();
        if inner.subscribers.len() >= limits.max_subscriptions {
            return Err(SubscribeError::TooMany);
        }
        if inner
            .subscribers
            .iter()
            .filter(|subscriber| subscriber.plugin == plugin)
            .count()
            >= limits.max_subscribers_per_plugin
        {
            return Err(SubscribeError::TooMany);
        }

        let mut replay = Replay::default();
        let cursor = match last_event_id {
            Some(last) => match inner.front_seq() {
                // The next expected row is gone; restart at the front.
                Some(front) if last.saturating_add(1) < front => {
                    replay.reset = true;
                    front.saturating_sub(1)
                }
                _ => last,
            },
            None => inner.latest_seq(),
        };
        if !replay.reset {
            replay.replayed = inner
                .events
                .iter()
                .filter(|event| event.seq > cursor && event.plugin == plugin)
                .count();
        }

        let id = self
            .next_sub
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        inner.subscribers.push(Subscriber {
            id,
            plugin: plugin.to_string(),
        });
        drop(inner);
        Ok((
            Subscription {
                bus: Arc::clone(self),
                plugin: plugin.to_string(),
                id,
                cursor,
            },
            replay,
        ))
    }

    fn unsubscribe(&self, id: u64) {
        self.lock()
            .subscribers
            .retain(|subscriber| subscriber.id != id);
    }

    /// Drop a purged plugin's rows and subscribers. Rows of other plugins stay
    /// where they are; the gaps are harmless because readers seek by seq.
    pub fn forget(&self, plugin: &str) {
        let mut inner = self.lock();
        inner.events.retain(|event| event.plugin != plugin);
        inner.bytes = inner
            .events
            .iter()
            .map(|event| event.json.len() + event.plugin.len())
            .sum();
        inner
            .subscribers
            .retain(|subscriber| subscriber.plugin != plugin);
    }

    pub fn publish(&self, plugin: &str, method: &str, params: &Value) {
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let event = Event {
            seq,
            plugin: plugin.to_string(),
            json: json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            })
            .to_string(),
        };
        let watching = {
            let mut inner = self.lock();
            let watching = inner
                .subscribers
                .iter()
                .any(|subscriber| subscriber.plugin == plugin);
            inner.bytes = inner.bytes.saturating_add(event.json.len());
            inner.events.push_back(event);
            inner.trim();
            watching
        };
        // The subscriber's handler is serviced by the connection driver, which
        // otherwise only looks at the bus on its poll deadline. Waking it here
        // turns a delivered event into one loop iteration instead of one
        // cadence period; without a wake (thread driver) nothing changes.
        if watching {
            self.wake();
        }
    }

    /// Install the driver's wake-up. Called once, when the driver starts.
    /// The thread-per-connection host cannot be woken, so nothing calls it
    /// there and the handler's own poll cadence delivers the event.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn set_wake(&self, wake: Wake) {
        *self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(wake);
    }

    fn wake(&self) {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    #[cfg(test)]
    pub fn subscriber_count(&self, plugin: &str) -> usize {
        self.lock()
            .subscribers
            .iter()
            .filter(|subscriber| subscriber.plugin == plugin)
            .count()
    }

    #[cfg(test)]
    pub fn history_len(&self, plugin: &str) -> usize {
        self.lock()
            .events
            .iter()
            .filter(|event| event.plugin == plugin)
            .count()
    }

    #[cfg(test)]
    pub fn ring_bytes(&self) -> usize {
        self.lock().bytes
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
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
    crate::wire::sse::SseEvent::new("message", &event.json)
        .with_id(event.seq)
        .encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> crate::platform::limits::Limits {
        crate::platform::limits::test_default()
    }

    #[test]
    fn event_bus_fans_out_and_limits_subscribers() {
        let bus = Arc::new(EventBus::new());
        let mut limits = limits();
        limits.max_subscribers_per_plugin = 2;
        limits.max_subscriptions = 3;

        let mut first = bus.subscribe("demo", &limits).expect("first");
        let mut second = bus.subscribe("demo", &limits).expect("second");
        assert_eq!(bus.subscriber_count("demo"), 2);
        assert!(matches!(
            bus.subscribe("demo", &limits),
            Err(SubscribeError::TooMany)
        ));
        // The per-plugin cap refused the third demo subscriber; another plugin
        // may still take the last slot, and then the global cap binds.
        let _third = bus.subscribe("other", &limits).expect("third");
        assert!(matches!(
            bus.subscribe("another", &limits),
            Err(SubscribeError::TooMany)
        ));

        bus.publish("demo", "notifications/message", &json!({"data": "one"}));
        for subscriber in [&mut first, &mut second] {
            match subscriber.poll() {
                Polled::Event(event) => assert!(event.json.contains("\"one\"")),
                other => panic!("expected the published event, got {other:?}"),
            }
            assert!(matches!(subscriber.poll(), Polled::Empty));
        }
    }

    #[test]
    fn forget_drops_one_plugins_rows_and_subscribers() {
        let bus = Arc::new(EventBus::new());
        let _subscription = bus.subscribe("demo", &limits()).expect("subscribe");
        let _keep = bus.subscribe("keep", &limits()).expect("subscribe");
        bus.publish("demo", "notifications/message", &json!({"data": "one"}));
        bus.publish("keep", "notifications/message", &json!({"data": "two"}));
        assert!(bus.history_len("demo") > 0);

        bus.forget("demo");
        assert_eq!(bus.subscriber_count("demo"), 0);
        assert_eq!(bus.history_len("demo"), 0);
        assert_eq!(bus.history_len("keep"), 1);
    }

    #[test]
    fn event_bus_replays_after_last_event_id() {
        let bus = Arc::new(EventBus::new());
        let limits = limits();
        let method = "notifications/message";
        bus.publish("replay", method, &json!({"data": "one"}));
        bus.publish("replay", method, &json!({"data": "two"}));
        bus.publish("replay", method, &json!({"data": "three"}));
        // Another plugin's rows share the ring and must not be replayed.
        bus.publish("other", method, &json!({"data": "skip"}));

        let (mut subscription, replay) = bus
            .subscribe_from("replay", Some(1), &limits)
            .expect("subscribe");
        assert!(!replay.reset);
        assert_eq!(replay.replayed, 2);
        let mut seen = Vec::new();
        while let Polled::Event(event) = subscription.poll() {
            seen.push(event.seq);
        }
        assert_eq!(seen, vec![2, 3]);

        bus.publish("replay", method, &json!({"data": "four"}));
        match subscription.poll() {
            Polled::Event(event) => assert_eq!(event.seq, 5),
            other => panic!("expected the live event, got {other:?}"),
        }
    }

    #[test]
    fn event_bus_marks_a_gap_when_last_event_id_falls_out() {
        let bus = Arc::new(EventBus::new());
        let limits = limits();
        for index in 0..(HISTORY_EVENTS + 5) {
            bus.publish("gap", "notifications/message", &json!({"n": index}));
        }
        assert_eq!(bus.history_len("gap"), HISTORY_EVENTS);
        let (subscription, replay) = bus
            .subscribe_from("gap", Some(1), &limits)
            .expect("subscribe");
        assert!(replay.reset);
        drop(subscription);
    }

    #[test]
    fn a_slow_subscriber_resyncs_instead_of_disconnecting() {
        let bus = Arc::new(EventBus::new());
        let mut slow = bus.subscribe("chatty", &limits()).expect("subscribe");
        // The subscriber never reads while the ring rolls past its rows; a
        // per-subscriber queue would have dropped it here.
        for index in 0..(HISTORY_EVENTS + 10) {
            bus.publish("chatty", "notifications/message", &json!({"n": index}));
        }
        assert!(matches!(slow.poll(), Polled::Reset));
        match slow.poll() {
            Polled::Event(event) => assert_eq!(event.seq, 11),
            other => panic!("expected the front of the ring after the reset, got {other:?}"),
        }
        assert_eq!(bus.subscriber_count("chatty"), 1);
    }

    #[test]
    fn the_ring_stays_within_its_byte_budget_whatever_the_subscribers() {
        let bus = Arc::new(EventBus::new());
        let limits = limits();
        let _subscribers: Vec<_> = (0..4)
            .map(|index| {
                bus.subscribe(&format!("p{index}"), &limits)
                    .expect("subscribe")
            })
            .collect();
        for _ in 0..1200 {
            bus.publish(
                "p0",
                "notifications/message",
                &json!({"data": "x".repeat(4096)}),
            );
        }
        assert!(
            bus.ring_bytes() <= HISTORY_BYTES,
            "ring grew to {} bytes",
            bus.ring_bytes()
        );
    }

    #[test]
    fn a_published_event_wakes_the_installed_driver() {
        use std::sync::atomic::AtomicUsize;

        let bus = Arc::new(EventBus::new());
        let _subscription = bus.subscribe("demo", &limits()).expect("subscribe");
        let wakes = Arc::new(AtomicUsize::new(0));
        {
            let wakes = wakes.clone();
            bus.set_wake(Arc::new(move || {
                wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }));
        }

        bus.publish("demo", "notifications/message", &json!({"data": "one"}));
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A plugin nobody watches must not pay a wake-up.
        bus.publish("quiet", "notifications/message", &json!({"data": "one"}));
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn sse_events_have_id_event_and_data() {
        let text = sse_event(&Event {
            seq: 7,
            plugin: "demo".into(),
            json: "{\"jsonrpc\":\"2.0\"}".into(),
        });
        assert_eq!(
            text,
            "id: 7\nevent: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n"
        );
    }
}
