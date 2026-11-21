#![cfg_attr(not(feature = "http"), allow(dead_code))]
#![cfg_attr(all(unix, not(test)), allow(dead_code))]
//! UI ↔ plugin realtime channel: injected event bus, SSE and WebSocket.
//!
//! Plugin stdout stays a JSON-RPC NDJSON control channel. A plugin's
//! `notifications/*` frames are mirrored to UI subscribers; requests from the
//! UI still go through the same `tools/call` semantics as
//! `POST /plugin-ui/{id}/rpc`. The HTTP layer owns authentication and Origin
//! checks; this module owns the transports and the bounded fan-out.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

const QUEUE: usize = 512;
const HISTORY_EVENTS: usize = 256;
const HISTORY_BYTES: usize = 2 * 1024 * 1024;

/// One notification delivered to UI subscribers. `seq` is monotonic across the
/// process and is used as the SSE event id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub seq: u64,
    pub json: String,
}

/// What a reactor installs so a published event does not wait for the loop's
/// poll cadence. Called from the publishing thread, never from the loop.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub struct EventBus {
    inner: Mutex<HashMap<String, PluginState>>,
    next_seq: AtomicU64,
    next_sub: AtomicU64,
    total_subscribers: AtomicU64,
    /// Installed by the host's connection driver. Absent on a driver that
    /// cannot be woken (the thread-per-connection fallback), where the
    /// handler's own poll cadence delivers the event instead.
    wake: Mutex<Option<Wake>>,
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

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            inner: Mutex::new(HashMap::new()),
            next_seq: AtomicU64::new(0),
            next_sub: AtomicU64::new(0),
            total_subscribers: AtomicU64::new(0),
            wake: Mutex::new(None),
        }
    }

    pub fn subscribe(
        self: &Arc<Self>,
        plugin: &str,
        limits: &crate::platform::limits::Limits,
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
        limits: &crate::platform::limits::Limits,
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

    #[cfg(test)]
    pub fn subscribe_with(
        self: &Arc<Self>,
        plugin: &str,
        sender: SyncSender<Event>,
        limits: &crate::platform::limits::Limits,
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

    fn reserve_subscriber(
        &self,
        limits: &crate::platform::limits::Limits,
    ) -> Result<(), SubscribeError> {
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
        let watching = !state.subscribers.is_empty();
        state
            .subscribers
            .retain(|(_, sender)| match sender.try_send(event.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            });
        drop(inner);
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
            .get(plugin)
            .map(|state| state.subscribers.len())
            .unwrap_or(0)
    }

    #[cfg(test)]
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
    crate::wire::sse::SseEvent::new("message", &event.json)
        .with_id(event.seq)
        .encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_fans_out_and_limits_subscribers() {
        let bus = Arc::new(EventBus::new());
        let limits = crate::platform::limits::test_default();
        let (receiver, subscription) = bus
            .subscribe("demo", &crate::platform::limits::test_default())
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
    fn two_event_buses_do_not_share_history_or_subscribers() {
        let first = Arc::new(EventBus::new());
        let second = Arc::new(EventBus::new());
        let limits = crate::platform::limits::test_default();
        let (receiver, subscription) = second.subscribe("demo", &limits).expect("subscribe");
        first.publish("demo", "notifications/message", &json!({"data": "one"}));
        assert_eq!(first.history_len("demo"), 1);
        assert_eq!(second.history_len("demo"), 0);
        assert!(
            receiver.try_recv().is_err(),
            "the first bus must not wake a subscriber of the second"
        );
        drop(subscription);
    }

    #[test]
    fn event_bus_forgets_a_purged_plugin() {
        let bus = Arc::new(EventBus::new());
        let (_receiver, subscription) = bus
            .subscribe("demo", &crate::platform::limits::test_default())
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
            .subscribe_from("replay", Some(1), &crate::platform::limits::test_default())
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
            .subscribe_from("gap", Some(1), &crate::platform::limits::test_default())
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
    fn a_published_event_wakes_the_installed_driver() {
        use std::sync::atomic::AtomicUsize;

        let bus = Arc::new(EventBus::new());
        let limits = crate::platform::limits::test_default();
        let (_receiver, subscription) = bus.subscribe("demo", &limits).expect("subscribe");
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
}
