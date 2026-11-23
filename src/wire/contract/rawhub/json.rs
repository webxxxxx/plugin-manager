//! Stream-0 helpers: the JSON-RPC half of the protocol-3 transport (#265).
//!
//! Kept in its own file so `rawhub.rs` stays inside the per-file line budget;
//! both `impl` blocks are inherent methods of the types in the parent module.

use super::*;

impl RawStats {
    /// Account one stream-0 frame that made it to the JSON channel.
    pub(super) fn note_json(&self, bytes: usize) {
        self.json_frames.fetch_add(1, Ordering::Relaxed);
        self.json_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

impl RawHub {
    /// One stream-0 payload from the plugin's stdout.
    ///
    /// The raw header already delimits the message, so the payload is the JSON
    /// text itself. A frame that breaks the JSON contract (bad UTF-8, over the
    /// call limit, or a full queue) is reported as a JSON-channel failure; a
    /// full queue means the instance kept answering after the slot stopped
    /// draining, so it is closed like any other protocol violation.
    pub(super) fn json_frame(&self, plugin: &RawPlugin, payload: Vec<u8>) {
        let Some(sender) = plugin.json.clone() else {
            self.stats
                .record(RawFailure::JsonChannelClosed, &plugin.id, 0, "");
            return;
        };
        if payload.len() > self.json_max {
            self.stats
                .record(RawFailure::JsonFrameTooLarge, &plugin.id, 0, "");
            let _ = sender.try_send(FrameEvent::Frame(Err(FrameError::TooLarge {
                max: self.json_max,
            })));
            return;
        }
        let event = match String::from_utf8(payload) {
            Ok(line) => {
                self.stats.note_json(line.len());
                FrameEvent::Frame(Ok(line))
            }
            Err(_) => FrameEvent::Frame(Err(FrameError::InvalidUtf8)),
        };
        match sender.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Backpressure, not a protocol violation: the slot drains this
                // queue every tick, so a full queue means the caller side is
                // busy. Dropping the frame keeps the instance alive; the call
                // waiting for it is bounded by the slot's pending table and its
                // call timeout (the same 503 path protocol 1/2 uses).
                self.stats
                    .record(RawFailure::JsonChannelFull, &plugin.id, 0, "");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Write one JSON-RPC frame on stream 0. The slot's pending table owns the
    /// call backpressure, so this path does not touch the raw in-flight budget
    /// or stream subscriptions.
    pub(crate) fn send_json(&self, id: &str, payload: &[u8]) -> Result<(), String> {
        let plugin = self
            .plugins
            .lock()
            .ok()
            .and_then(|plugins| plugins.get(id).cloned())
            .ok_or_else(|| "raw plugin is gone".to_string())?;
        if plugin.closed.load(Ordering::SeqCst) {
            self.stats.record(RawFailure::WriterClosed, id, 0, "json");
            return Err(RawFailure::WriterClosed.message().to_string());
        }
        if payload.len() > self.json_max {
            self.stats
                .record(RawFailure::JsonFrameTooLarge, id, 0, "write");
            return Err(RawFailure::JsonFrameTooLarge.message().to_string());
        }
        let mut frame = Vec::with_capacity(HEADER + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&0u32.to_be_bytes());
        frame.extend_from_slice(payload);
        self.write_frame(&plugin, frame)
            .map_err(|failure| failure.message().to_string())
    }
}
