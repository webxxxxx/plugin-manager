//! Plugin UI SSE as a sans-IO stream handler.
//!
//! The subscription, the replay ring and the heartbeat all keep their old
//! semantics; the difference is ownership. The handler never writes a socket:
//! it appends chunked bytes to the server's bounded output buffer and asks for
//! a shorter deadline so a heartbeat is written even when no plugin event
//! arrives.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::server::http::{
    CloseReason, OutBuffer, StreamAction, StreamEvent, StreamHandler, StreamStep,
};
use crate::uichannel::{Event, Replay, Subscription};

const HEAD: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
X-Accel-Buffering: no\r\n\
Transfer-Encoding: chunked\r\n\r\n";
const RESET: &[u8] = b"event: reset\ndata: {\"reason\":\"replay-unavailable\"}\n\n";
const PING: &[u8] = b": ping\n\n";
const HEARTBEAT: Duration = Duration::from_secs(15);
/// The server-loop poll cadence the legacy reactor used for bus events. A
/// handler asks for this wakeup so a published event is not held until the
/// heartbeat.
const POLL: Duration = Duration::from_millis(100);

pub(crate) struct SseStream {
    receiver: Receiver<Event>,
    _subscription: Subscription,
    replay: Replay,
    heartbeat: Instant,
    head_at: usize,
    /// One serialized SSE event, or the reset/ping marker. It may not fit in
    /// the output buffer in one writable event; bytes go out as one chunk.
    pending: Vec<u8>,
    pending_at: usize,
}

impl SseStream {
    pub(crate) fn new(
        receiver: Receiver<Event>,
        subscription: Subscription,
        replay: Replay,
    ) -> SseStream {
        SseStream {
            receiver,
            _subscription: subscription,
            replay,
            heartbeat: Instant::now(),
            head_at: 0,
            pending: Vec::new(),
            pending_at: 0,
        }
    }

    fn deadline(&self, now: Instant) -> Instant {
        let heartbeat = self.heartbeat + HEARTBEAT;
        let poll = now + POLL;
        let next = heartbeat.min(poll);
        if next > now {
            next
        } else {
            now + Duration::from_millis(1)
        }
    }

    fn step(&self, now: Instant) -> StreamStep {
        // NeedRead lets the Windows fallback block on the socket (and notice a
        // disconnect) while it wakes every POLL interval. The Unix loop watches
        // POLLIN for a Stream connection regardless of the action.
        StreamStep::wait(StreamAction::NeedRead).within(self.deadline(now))
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        let now = Instant::now();
        if self.head_at < HEAD.len() {
            let take = (HEAD.len() - self.head_at).min(out.remaining());
            out.write(&HEAD[self.head_at..self.head_at + take]);
            self.head_at += take;
            if self.head_at < HEAD.len() {
                return StreamStep::wait(StreamAction::NeedWrite).within(self.deadline(now));
            }
        }
        loop {
            if !self.pending.is_empty() && !self.flush_chunk(out) {
                return StreamStep::wait(StreamAction::NeedWrite).within(self.deadline(now));
            }
            if !self.pending.is_empty() {
                self.pending.clear();
                self.pending_at = 0;
            }
            if self.replay.reset {
                self.replay.reset = false;
                self.pending = RESET.to_vec();
                continue;
            }
            match self.receiver.try_recv() {
                Ok(event) => {
                    self.pending = crate::uichannel::sse_event(&event).into_bytes();
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
                }
            }
            if self.heartbeat.elapsed() >= HEARTBEAT {
                self.heartbeat = now;
                self.pending = PING.to_vec();
                continue;
            }
            break;
        }
        if out.remaining() < 32 {
            return StreamStep::wait(StreamAction::NeedWrite).within(self.deadline(now));
        }
        self.step(now)
    }

    /// Write the next chunk of `pending`. Returns true when the whole event
    /// has been handed to the output buffer.
    fn flush_chunk(&mut self, out: &mut OutBuffer<'_>) -> bool {
        let remaining = self.pending.len() - self.pending_at;
        // "len\r\n" + payload + "\r\n"
        let overhead = 10 + 4;
        if out.remaining() <= overhead {
            return false;
        }
        let take = remaining.min(out.remaining() - overhead);
        if take == 0 {
            return false;
        }
        let header = format!("{:x}\r\n", take);
        out.write(header.as_bytes());
        out.write(&self.pending[self.pending_at..self.pending_at + take]);
        out.write(b"\r\n");
        self.pending_at += take;
        self.pending_at >= self.pending.len()
    }
}

impl StreamHandler for SseStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Tick(now) => {
                if now.saturating_duration_since(self.heartbeat) >= HEARTBEAT {
                    self.heartbeat = now;
                    self.pending = PING.to_vec();
                }
                // A tick is also the server's wakeup for a handler with no
                // socket event: write the heartbeat and drain the bus.
                self.on_writable(out)
            }
            // The client should not send body bytes on an SSE GET; consuming
            // them keeps the connection alive without echoing anything.
            StreamEvent::Data(_) | StreamEvent::BlockingDone(_) => self.step(Instant::now()),
            StreamEvent::Closed(reason) => StreamStep::wait(StreamAction::Close(reason)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uichannel::EventBus;
    use serde_json::json;
    use std::sync::Arc;

    fn harness() -> (Arc<EventBus>, SseStream) {
        let bus = Arc::new(EventBus::new());
        let (receiver, subscription, replay) = bus.subscribe_from("demo", None).expect("subscribe");
        (bus, SseStream::new(receiver, subscription, replay))
    }

    fn drive(stream: &mut SseStream, limit: usize, events: usize) -> Vec<u8> {
        let mut all = Vec::new();
        for _ in 0..events {
            let mut out = Vec::new();
            let step = stream.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
            assert!(
                out.len() <= limit,
                "the handler must respect the buffer limit"
            );
            all.extend_from_slice(&out);
            if matches!(step.action, StreamAction::Close(_)) {
                break;
            }
        }
        all
    }

    /// Decode an HTTP chunked body into the concatenated payload.
    fn dechunk(stream: &[u8]) -> Vec<u8> {
        let body_start = stream
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|end| end + 4)
            .unwrap_or(0);
        let mut body = &stream[body_start..];
        let mut decoded = Vec::new();
        loop {
            let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") else {
                break;
            };
            let size = std::str::from_utf8(&body[..line_end]).expect("chunk size");
            let size = usize::from_str_radix(size, 16).expect("hex");
            body = &body[line_end + 2..];
            if size == 0 {
                break;
            }
            decoded.extend_from_slice(&body[..size]);
            body = &body[size + 2..];
        }
        decoded
    }

    #[test]
    fn an_event_is_written_as_a_chunked_sse_record() {
        let (bus, mut stream) = harness();
        let bytes = drive(&mut stream, 4096, 1);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(
            text.contains("Content-Type: text/event-stream\r\n"),
            "{text}"
        );
        assert!(text.contains("Transfer-Encoding: chunked\r\n"), "{text}");
        bus.publish("demo", "notifications/message", &json!({"data": "hello"}));
        let bytes = drive(&mut stream, 4096, 1);
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        assert!(body.contains("event: message"), "{body}");
        assert!(body.contains("\"data\":\"hello\""), "{body}");
        assert!(body.contains("id: "), "{body}");
    }

    #[test]
    fn a_replay_gap_writes_the_reset_event_first() {
        let bus = Arc::new(EventBus::new());
        let (receiver, subscription, _replay) =
            bus.subscribe_from("demo", None).expect("subscribe");
        let mut stream = SseStream::new(
            receiver,
            subscription,
            Replay {
                reset: true,
                replayed: 0,
            },
        );
        let bytes = drive(&mut stream, 4096, 1);
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        assert!(body.starts_with("event: reset"), "{body}");
    }

    #[test]
    fn a_tick_writes_the_heartbeat_without_a_plugin_event() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096, 1);
        let now = Instant::now() + HEARTBEAT + Duration::from_secs(1);
        let mut out = Vec::new();
        stream.on_event(StreamEvent::Tick(now), &mut OutBuffer::new(&mut out, 4096));
        let body = String::from_utf8(dechunk(&out)).expect("utf8");
        assert_eq!(body, ": ping\n\n");
    }

    #[test]
    fn a_small_output_buffer_splits_an_event_without_losing_bytes() {
        let (bus, mut stream) = harness();
        bus.publish(
            "demo",
            "notifications/message",
            &json!({"data": "x".repeat(200)}),
        );
        let mut all = Vec::new();
        for _ in 0..200 {
            let mut out = Vec::new();
            stream.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 32));
            all.extend_from_slice(&out);
        }
        let body = String::from_utf8(dechunk(&all)).expect("utf8");
        assert_eq!(body.matches('x').count(), 200, "{body}");
        assert!(body.ends_with("\n\n"), "{body}");
    }

    #[test]
    fn a_closed_connection_releases_the_subscription() {
        let (bus, mut stream) = harness();
        let step = stream.on_event(
            StreamEvent::Closed(CloseReason::PeerClosed),
            &mut OutBuffer::new(&mut Vec::new(), 0),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
        drop(stream);
        assert_eq!(bus.subscriber_count("demo"), 0);
    }
}
