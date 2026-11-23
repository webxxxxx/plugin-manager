//! The raw transport's control stream: per-stream close frames (#266).
//!
//! A protocol-3 plugin that only ever wrote data could never learn that a
//! `/raw` subscriber had gone away, and a subscriber could only learn that the
//! plugin was done when its process exited. The control stream carries that
//! signal in both directions:
//!
//! ```text
//! stream 0xFFFF_FFFF | {"op":"close","stream":N}
//! ```
//!
//! The frame header is the same one data streams use, so the signal travels
//! through the same writer queue and backpressure. Data stream payloads are
//! unchanged: an empty payload is still an empty data frame.

use serde_json::{json, Value};

use super::*;

/// Reserved stream id for transport control frames. Data streams are
/// `1 ..= CONTROL_STREAM - 1`.
pub const CONTROL_STREAM: u32 = u32::MAX;

/// A control frame is one small JSON object; anything larger is a broken peer.
const CONTROL_MAX: usize = 128;

/// Who ended a stream. Only the plugin-initiated close deserves a log line:
/// a subscriber leaving is already logged by the WebSocket side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClosedBy {
    Subscriber,
    Plugin,
    /// A raw stream produced a frame over `ws_raw_max_bytes`; only that stream
    /// ends, the instance keeps running.
    Oversized,
}

impl RawStats {
    /// Account one raw stream that either side closed.
    pub(super) fn note_stream_closed(&self) {
        self.streams_closed.fetch_add(1, Ordering::Relaxed);
    }
}

impl RawHub {
    /// Data for a stream whose subscriber has left: report it and answer the
    /// late write with a close frame instead of dropping it silently. The
    /// plugin may have written before it read the close we sent on drop.
    pub(super) fn late_stream_write(&self, plugin: &RawPlugin, stream: u32) {
        self.stats.record(
            RawFailure::StreamNotSubscribed,
            &plugin.id,
            stream,
            "late write",
        );
        self.send_control_close(plugin, stream);
    }

    /// One control payload from the plugin's stdout.
    pub(super) fn control_frame(&self, plugin: &RawPlugin, payload: Vec<u8>) {
        if payload.len() > CONTROL_MAX {
            self.invalid_control(plugin, "oversized");
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(&payload) else {
            self.invalid_control(plugin, "not json");
            return;
        };
        let close = match (value.get("op").and_then(Value::as_str), value.get("stream")) {
            (Some("close"), Some(stream)) => stream
                .as_u64()
                .filter(|stream| *stream > 0 && *stream < u64::from(CONTROL_STREAM)),
            _ => None,
        };
        match close {
            Some(stream) => self.close_stream(plugin, stream as u32, ClosedBy::Plugin),
            None => self.invalid_control(plugin, "unknown op"),
        }
    }

    fn invalid_control(&self, plugin: &RawPlugin, detail: &str) {
        self.stats.record(
            RawFailure::ControlFrameInvalid,
            &plugin.id,
            CONTROL_STREAM,
            detail,
        );
    }

    /// Tell the plugin one of its streams is gone. Idempotent and best effort:
    /// a full writer queue drops the frame and counts it like any other write.
    pub(super) fn send_control_close(&self, plugin: &RawPlugin, stream: u32) {
        let payload = json!({"op": "close", "stream": stream}).to_string();
        let mut frame = Vec::with_capacity(HEADER + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&CONTROL_STREAM.to_be_bytes());
        frame.extend_from_slice(payload.as_bytes());
        if let Err(failure) = self.write_frame(plugin, frame) {
            self.stats
                .record(failure, &plugin.id, CONTROL_STREAM, "control");
        }
    }
}

impl RawHub {
    /// Drop one stream's subscriber and let the plugin know. A close for a
    /// stream nobody subscribed to is a no-op: the plugin is telling us
    /// something we already acted on.
    pub(super) fn close_stream(&self, plugin: &RawPlugin, stream: u32, by: ClosedBy) {
        let sender = plugin
            .subscribers
            .lock()
            .ok()
            .and_then(|mut subscribers| subscribers.remove(&stream));
        let Some(sender) = sender else {
            return;
        };
        let _ = sender.try_send(RawEvent::Closed);
        self.release_stream(plugin, stream);
        self.stats.note_stream_closed();
        match by {
            ClosedBy::Subscriber => {
                // The plugin has to learn that the stream is gone, otherwise it
                // keeps writing into a stream nobody reads.
                self.send_control_close(plugin, stream);
            }
            ClosedBy::Plugin => {
                info!("{}: raw stream {} closed by the plugin", plugin.id, stream);
            }
            ClosedBy::Oversized => {
                warn!(
                    "{}: raw stream {} closed: frame over the size limit",
                    plugin.id, stream
                );
                self.send_control_close(plugin, stream);
            }
        }
    }
}
