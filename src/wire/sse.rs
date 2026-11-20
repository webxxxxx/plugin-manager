//! Server-Sent Events text framing.
//!
//! One event is `id:` (optional), `event:` and `data:` lines followed by a
//! blank line. The chunked variant wraps the same bytes in one HTTP chunk so a
//! stream handler and the Windows blocking fallback cannot drift.

/// One SSE event, borrowed from the caller's JSON/line buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SseEvent<'a> {
    pub id: Option<u64>,
    pub event: &'a str,
    pub data: &'a str,
}

impl<'a> SseEvent<'a> {
    pub(crate) fn new(event: &'a str, data: &'a str) -> SseEvent<'a> {
        SseEvent {
            id: None,
            event,
            data,
        }
    }

    pub(crate) fn with_id(mut self, id: u64) -> SseEvent<'a> {
        self.id = Some(id);
        self
    }

    /// The text payload, including the terminating blank line.
    pub(crate) fn encode(&self) -> String {
        let mut out = String::new();
        if let Some(id) = self.id {
            out.push_str(&format!("id: {}\n", id));
        }
        out.push_str(&format!("event: {}\ndata: {}\n\n", self.event, self.data));
        out
    }

    /// The same payload as one HTTP `Transfer-Encoding: chunked` chunk.
    pub(crate) fn chunked(&self) -> Vec<u8> {
        let payload = self.encode();
        let mut bytes = format!("{:x}\r\n", payload.len()).into_bytes();
        bytes.extend_from_slice(payload.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::SseEvent;

    #[test]
    fn an_event_without_an_id_is_event_plus_data() {
        let text = SseEvent::new("message", "{\"ok\":true}").encode();
        assert_eq!(text, "event: message\ndata: {\"ok\":true}\n\n");
    }

    #[test]
    fn an_id_comes_first_and_a_chunk_is_length_prefixed() {
        let event = SseEvent::new("message", "x").with_id(7);
        assert_eq!(event.encode(), "id: 7\nevent: message\ndata: x\n\n");
        let chunk = event.chunked();
        assert!(chunk.starts_with(b"1e\r\nid: 7\nevent: message\ndata: x\n\n\r\n"));
    }
}
