//! One server frame waiting for output-buffer capacity.
//!
//! A notification or a JSON-RPC response can be larger than the configured
//! stream buffer. The handler keeps the header and payload it already owns and
//! writes them across consecutive writable events; it never replaces the
//! server's buffer with an unbounded one.

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
