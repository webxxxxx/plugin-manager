//! `POST /mcp` progress over SSE as a sans-IO stream handler.
//!
//! The transport in `mcp_http` still validates the request and resolves the
//! principal; this handler owns the part that outlives the request head: the
//! chunked SSE response, the upstream progress events, the final result and
//! the cancellation of the outbound management call. It never touches a
//! socket, so it is driven by the server's `StreamDriver` exactly like the
//! plugin UI and blob handlers.
//!
//! Ordering is the old ordering: progress notifications are written as they
//! arrive, then the final JSON-RPC response, then the terminating chunk. The
//! receiver is the reactor's bounded queue (64 events), so a slow client
//! backpressures the upstream socket instead of growing a handler-side queue;
//! at most one unflushed event is held here, and the reactor refuses frames
//! larger than its IPC frame limit, so that event is bounded.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::server::http::{
    CloseReason, OutBuffer, StreamAction, StreamClass, StreamEvent, StreamHandler, StreamStep,
};
use crate::server::mcp::result::finish_modern_tool_call;
use crate::wire::control::channel::ProgressEvent;
use crate::wire::message::rpc;

/// The legacy poll cadence of the server loop. Waiting for the next event
/// schedules a tick this far ahead instead of waiting for the transfer bound.
const POLL: Duration = Duration::from_millis(100);

const HEAD: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\
Transfer-Encoding: chunked\r\n\r\n";
const END: &[u8] = b"0\r\n\r\n";

/// Cancel one outbound streaming call. The server loop uses the reactor; a
/// unit test uses a recording closure.
pub type Cancel = Box<dyn Fn(u64) + Send>;

/// The chunked SSE response for one streaming `tools/call`.
pub struct McpSseStream {
    receiver: Receiver<ProgressEvent>,
    id: Option<Value>,
    stream: u64,
    /// Taken on the first close so a drop after an explicit close cannot
    /// cancel the same upstream stream twice.
    cancel: Option<Cancel>,
    head_at: usize,
    /// One encoded chunk (or the terminating chunk) that did not fit in the
    /// server's output buffer yet.
    pending: Vec<u8>,
    pending_at: usize,
    /// The final response line was queued; close once `pending` is drained.
    finishing: bool,
    /// The terminating chunk is on the wire.
    done: bool,
    /// The upstream stream already delivered its result; cancelling it is
    /// pointless and would only log a missing stream id.
    final_seen: bool,
}

impl McpSseStream {
    pub fn new(
        receiver: Receiver<ProgressEvent>,
        id: Option<Value>,
        stream: u64,
        cancel: Cancel,
    ) -> McpSseStream {
        McpSseStream {
            receiver,
            id,
            stream,
            cancel: Some(cancel),
            head_at: 0,
            pending: Vec::new(),
            pending_at: 0,
            finishing: false,
            done: false,
            final_seen: false,
        }
    }

    fn cancel_upstream(&mut self) {
        if self.final_seen {
            return;
        }
        // The final frame may already be queued but not written yet: the
        // reactor removes the stream when it delivers that frame, so
        // cancelling it would target an id that is already gone. Drain only
        // to find out; the queued progress frames are moot once the client
        // disconnected.
        while let Ok(event) = self.receiver.try_recv() {
            if matches!(event, ProgressEvent::Final(_)) {
                self.final_seen = true;
                break;
            }
        }
        if self.final_seen {
            return;
        }
        if let Some(cancel) = self.cancel.take() {
            cancel(self.stream);
        }
    }

    fn advance(&mut self, out: &mut OutBuffer<'_>, now: Instant) -> StreamStep {
        if self.done {
            return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
        }
        if self.head_at < HEAD.len() {
            let take = (HEAD.len() - self.head_at).min(out.remaining());
            if take > 0 {
                out.write(&HEAD[self.head_at..self.head_at + take]);
                self.head_at += take;
            }
            if self.head_at < HEAD.len() {
                // The head did not fit: let the socket drain. No short wakeup
                // here, so a client that never reads still hits the server's
                // no-progress bound.
                return StreamStep::wait(StreamAction::NeedWrite);
            }
        }
        loop {
            if self.pending_at < self.pending.len() {
                let take = (self.pending.len() - self.pending_at).min(out.remaining());
                if take > 0 {
                    out.write(&self.pending[self.pending_at..self.pending_at + take]);
                    self.pending_at += take;
                }
                if self.pending_at < self.pending.len() {
                    return StreamStep::wait(StreamAction::NeedWrite);
                }
            }
            self.pending.clear();
            self.pending_at = 0;
            if self.finishing {
                self.done = true;
                return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
            }
            match self.receiver.try_recv() {
                Ok(event) => {
                    self.queue(event);
                    continue;
                }
                Err(TryRecvError::Empty) => {
                    return StreamStep::wait(StreamAction::NeedRead).within(now + POLL);
                }
                Err(TryRecvError::Disconnected) => {
                    // The reactor drops its sender when the management
                    // connection ends without a final frame. Answer with the
                    // same -32603 the old engine used for a broken channel
                    // instead of leaving the SSE stream open forever.
                    self.queue(ProgressEvent::Error(
                        "management stream closed before the final result".into(),
                    ));
                    continue;
                }
            }
        }
    }

    fn queue(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::Frame(line) => {
                self.pending = sse_chunk(&line);
            }
            ProgressEvent::Final(line) => {
                let response = serde_json::from_str::<rpc::Response>(&line).unwrap_or_else(|_| {
                    rpc::err(
                        self.id.clone(),
                        -32603,
                        "invalid response from management channel",
                    )
                });
                self.pending = final_chunk(self.id.clone(), response);
                self.finishing = true;
                self.final_seen = true;
            }
            ProgressEvent::Error(message) => {
                let response = rpc::err(self.id.clone(), -32603, &message);
                self.pending = final_chunk(self.id.clone(), response);
                self.finishing = true;
                self.final_seen = true;
            }
        }
    }
}

impl StreamHandler for McpSseStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable | StreamEvent::Tick(_) => self.advance(out, Instant::now()),
            StreamEvent::Data(_) | StreamEvent::BlockingDone(_) => {
                StreamStep::wait(StreamAction::NeedRead).within(Instant::now() + POLL)
            }
            StreamEvent::Closed(reason) => {
                self.cancel_upstream();
                StreamStep::wait(StreamAction::Close(reason))
            }
        }
    }

    fn stream_class(&self) -> StreamClass {
        // One MCP SSE response is a long-lived stream and draws from the
        // shared `max_streams` budget, exactly like a plugin SSE stream.
        StreamClass::LongLived
    }
}

impl Drop for McpSseStream {
    fn drop(&mut self) {
        // The driver delivers `Closed` before dropping a handler, but a
        // handler dropped directly (a test or a rejected attach) must release
        // the outbound call too.
        self.cancel_upstream();
    }
}

fn sse_chunk(line: &str) -> Vec<u8> {
    crate::wire::sse::SseEvent::new("message", line).chunked()
}

fn final_chunk(id: Option<Value>, response: rpc::Response) -> Vec<u8> {
    let response = finish_modern_tool_call(id, response);
    let mut bytes = sse_chunk(&rpc::to_line(&response));
    bytes.extend_from_slice(END);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::{Arc, Mutex};

    struct Harness {
        sender: SyncSender<ProgressEvent>,
        stream: McpSseStream,
        cancels: Arc<Mutex<Vec<u64>>>,
    }

    fn harness() -> Harness {
        let (sender, receiver) = sync_channel(8);
        let cancels = Arc::new(Mutex::new(Vec::new()));
        let seen = cancels.clone();
        let stream = McpSseStream::new(
            receiver,
            Some(serde_json::json!(7)),
            42,
            Box::new(move |id| seen.lock().expect("cancels").push(id)),
        );
        Harness {
            sender,
            stream,
            cancels,
        }
    }

    fn drive(stream: &mut McpSseStream, limit: usize, rounds: usize) -> (Vec<u8>, StreamStep) {
        let mut all = Vec::new();
        let mut last = StreamStep::wait(StreamAction::Continue);
        for _ in 0..rounds {
            let mut out = Vec::new();
            last = stream.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
            assert!(
                out.len() <= limit,
                "the handler must respect the buffer limit"
            );
            all.extend_from_slice(&out);
            if matches!(last.action, StreamAction::Close(_)) {
                break;
            }
        }
        (all, last)
    }

    fn dechunk(bytes: &[u8]) -> Vec<u8> {
        let body_start = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|end| end + 4)
            .expect("response head");
        let mut body = &bytes[body_start..];
        let mut decoded = Vec::new();
        loop {
            let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") else {
                break;
            };
            let size = std::str::from_utf8(&body[..line_end]).expect("chunk size");
            let size = usize::from_str_radix(size, 16).expect("hex chunk size");
            body = &body[line_end + 2..];
            if size == 0 {
                break;
            }
            decoded.extend_from_slice(&body[..size]);
            body = &body[size + 2..];
        }
        decoded
    }

    fn final_line(result: &str) -> String {
        rpc::to_line(&rpc::ok(
            Some(serde_json::json!(7)),
            serde_json::json!({
                "content": [{ "type": "text", "text": result }],
                "isError": false,
            }),
        ))
    }

    #[test]
    fn the_head_keeps_the_legacy_sse_wire_bytes() {
        let mut h = harness();
        let (bytes, step) = drive(&mut h.stream, 4096, 1);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(
            text.contains("Content-Type: text/event-stream\r\n"),
            "{text}"
        );
        assert!(text.contains("Cache-Control: no-cache\r\n"), "{text}");
        assert!(text.contains("Connection: keep-alive\r\n"), "{text}");
        assert!(text.contains("Transfer-Encoding: chunked\r\n"), "{text}");
        assert_eq!(text, String::from_utf8_lossy(HEAD));
        assert!(
            step.deadline.is_some(),
            "an idle handler asks for a short wakeup, not the transfer bound"
        );
    }

    #[test]
    fn progress_is_written_before_the_final_result() {
        let mut h = harness();
        let (mut bytes, _) = drive(&mut h.stream, 4096, 1);
        h.sender
            .send(ProgressEvent::Frame(
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"t1"}}"#
                    .into(),
            ))
            .expect("frame");
        h.sender
            .send(ProgressEvent::Final(final_line("pong")))
            .expect("final");
        let (tail, step) = drive(&mut h.stream, 4096, 20);
        bytes.extend_from_slice(&tail);
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        let progress = body.find("notifications/progress").expect("progress event");
        let final_at = body.find("\"pong\"").expect("final result");
        assert!(
            progress < final_at,
            "progress must arrive before the result: {body}"
        );
        assert!(body.contains("\"id\":7"), "{body}");
        assert!(body.contains("\"resultType\":\"complete\""), "{body}");
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.ends_with("0\r\n\r\n"),
            "the chunked body is terminated"
        );
    }

    #[test]
    fn an_outbound_error_becomes_the_legacy_internal_error_and_terminates() {
        let mut h = harness();
        h.sender
            .send(ProgressEvent::Error("svcd unreachable: gone".into()))
            .expect("error");
        let (bytes, step) = drive(&mut h.stream, 4096, 20);
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        assert!(body.contains("\"code\":-32603"), "{body}");
        assert!(body.contains("svcd unreachable: gone"), "{body}");
    }

    #[test]
    fn a_malformed_final_line_maps_to_an_internal_error() {
        let mut h = harness();
        h.sender
            .send(ProgressEvent::Final("not json".into()))
            .expect("final");
        let (bytes, _) = drive(&mut h.stream, 4096, 20);
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        assert!(
            body.contains("invalid response from management channel"),
            "{body}"
        );
    }

    #[test]
    fn a_disconnected_management_stream_finishes_instead_of_hanging() {
        let mut h = harness();
        drop(h.sender);
        let (bytes, step) = drive(&mut h.stream, 4096, 20);
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        assert!(
            body.contains("management stream closed before the final result"),
            "{body}"
        );
    }

    #[test]
    fn a_small_output_buffer_splits_the_body_without_losing_bytes() {
        let mut h = harness();
        let (mut bytes, _) = drive(&mut h.stream, 4096, 1);
        let progress = format!(
            r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{{"progress":1,"progressToken":"{}"}}}}"#,
            "Q".repeat(200)
        );
        h.sender
            .send(ProgressEvent::Frame(progress.clone()))
            .expect("frame");
        h.sender
            .send(ProgressEvent::Final(final_line("done")))
            .expect("final");
        let (tail, step) = drive(&mut h.stream, 24, 200);
        bytes.extend_from_slice(&tail);
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let body = String::from_utf8(dechunk(&bytes)).expect("utf8");
        assert_eq!(body.matches("Q".repeat(200).as_str()).count(), 1, "{body}");
        assert!(body.contains("notifications/progress"), "{body}");
        assert!(body.contains("\"done\""), "{body}");
    }

    #[test]
    fn a_blocked_output_keeps_the_no_progress_bound_armed() {
        let mut h = harness();
        let _ = drive(&mut h.stream, 4096, 1);
        h.sender
            .send(ProgressEvent::Frame(
                r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}"#
                    .into(),
            ))
            .expect("frame");
        let mut out = Vec::new();
        let step = h
            .stream
            .on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 8));
        assert_eq!(out.len(), 8, "the chunk is split at the buffer limit");
        assert!(matches!(step.action, StreamAction::NeedWrite));
        assert!(
            step.deadline.is_none(),
            "a blocked writer must not extend the server's transfer deadline"
        );
    }

    #[test]
    fn closing_before_the_final_result_cancels_the_upstream_call_once() {
        let mut h = harness();
        let step = h.stream.on_event(
            StreamEvent::Closed(CloseReason::PeerClosed),
            &mut OutBuffer::new(&mut Vec::new(), 0),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
        assert_eq!(*h.cancels.lock().expect("cancels"), vec![42]);
        drop(h.stream);
        assert_eq!(
            *h.cancels.lock().expect("cancels"),
            vec![42],
            "a drop after the close must not cancel twice"
        );
    }

    #[test]
    fn a_final_frame_waiting_in_the_queue_is_not_cancelled() {
        let mut h = harness();
        h.sender
            .send(ProgressEvent::Final(final_line("pong")))
            .expect("final");
        let step = h.stream.on_event(
            StreamEvent::Closed(CloseReason::PeerClosed),
            &mut OutBuffer::new(&mut Vec::new(), 0),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
        assert!(
            h.cancels.lock().expect("cancels").is_empty(),
            "a queued final means the reactor already finished the call"
        );
    }

    #[test]
    fn a_completed_stream_is_not_cancelled() {
        let mut h = harness();
        h.sender
            .send(ProgressEvent::Final(final_line("pong")))
            .expect("final");
        let _ = drive(&mut h.stream, 4096, 20);
        drop(h.stream);
        assert!(
            h.cancels.lock().expect("cancels").is_empty(),
            "the reactor already removed a finished stream"
        );
    }

    #[test]
    fn the_stream_is_long_lived() {
        let h = harness();
        assert_eq!(h.stream.stream_class(), StreamClass::LongLived);
    }
}
