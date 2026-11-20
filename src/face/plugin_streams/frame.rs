//! One server frame waiting for output-buffer capacity.
//!
//! A notification or a JSON-RPC response can be larger than the configured
//! stream buffer. The handler keeps the header and payload it already owns and
//! writes them across consecutive writable events; it never replaces the
//! server's buffer with an unbounded one.

use std::collections::VecDeque;

use crate::server::http::OutBuffer;
use crate::wire::ws::frame;

pub(crate) struct PendingFrame {
    header: Vec<u8>,
    header_at: usize,
    payload: Vec<u8>,
    payload_at: usize,
}

impl PendingFrame {
    pub(crate) fn new(opcode: u8, payload: Vec<u8>) -> PendingFrame {
        PendingFrame {
            header: frame::frame_header(opcode, payload.len()),
            header_at: 0,
            payload,
            payload_at: 0,
        }
    }

    pub(crate) fn text(text: String) -> PendingFrame {
        PendingFrame::new(0x1, text.into_bytes())
    }

    /// Write as much as fits. Returns true when the whole frame is queued in
    /// the output buffer.
    pub(crate) fn write(&mut self, out: &mut OutBuffer<'_>) -> bool {
        if self.header_at < self.header.len() {
            let take = (self.header.len() - self.header_at).min(out.remaining());
            out.write(&self.header[self.header_at..self.header_at + take]);
            self.header_at += take;
            if self.header_at < self.header.len() {
                return false;
            }
        }
        let take = (self.payload.len() - self.payload_at).min(out.remaining());
        out.write(&self.payload[self.payload_at..self.payload_at + take]);
        self.payload_at += take;
        self.payload_at >= self.payload.len()
    }
}

/// What one writable event managed to drain from the head, the queued frames
/// and the close frame.
pub(crate) enum QueuedWrite {
    /// The output buffer filled up; ask for another writable event.
    NeedWrite,
    /// The close frame is queued but an oversized inbound frame is still being
    /// drained, so keep reading.
    NeedRead,
    /// Everything, including the close frame, was written.
    Closed,
    /// The head and every queued frame fitted.
    Complete,
}

/// The shared head/frame/close drain used by the raw-WS and plugin-UI WS
/// handlers. `discard` is the handler's remaining oversized-frame byte count.
pub(crate) fn write_queued(
    head: &[u8],
    head_at: &mut usize,
    frames: &mut VecDeque<PendingFrame>,
    close: &mut Option<Vec<u8>>,
    closing: bool,
    discard: usize,
    out: &mut OutBuffer<'_>,
) -> QueuedWrite {
    if *head_at < head.len() {
        let take = (head.len() - *head_at).min(out.remaining());
        out.write(&head[*head_at..*head_at + take]);
        *head_at += take;
        if *head_at < head.len() {
            return QueuedWrite::NeedWrite;
        }
    }
    while let Some(frame) = frames.front_mut() {
        if !frame.write(out) {
            return QueuedWrite::NeedWrite;
        }
        frames.pop_front();
    }
    if closing {
        if let Some(payload) = close.take() {
            let mut frame = PendingFrame::new(0x8, payload);
            if !frame.write(out) {
                frames.push_front(frame);
                return QueuedWrite::NeedWrite;
            }
        }
        if discard > 0 {
            return QueuedWrite::NeedRead;
        }
        if frames.is_empty() {
            return QueuedWrite::Closed;
        }
    }
    QueuedWrite::Complete
}
