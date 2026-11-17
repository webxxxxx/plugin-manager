//! Plugin UI WebSocket as a sans-IO stream handler.
//!
//! The page gets notifications from the UI bus and sends `tools/call`
//! envelopes. Notifications are frames the handler writes itself; a call is a
//! control-channel round trip, so the handler submits it as a blocking job and
//! resumes on `BlockingDone`. Frame parsing, fragmentation limits, heartbeat
//! pings and the in-flight queue keep the behaviour the reactor had.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::channel;
use crate::rpc;
use crate::server::http::wire;
use crate::server::http::{
    BlockingJob, BlockingOutcome, CloseReason, OutBuffer, StreamAction, StreamEvent, StreamHandler,
    StreamStep,
};
use crate::uichannel::{Event, Subscription};
use crate::webui;

use super::frame::PendingFrame;

const WS_PING: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);

pub(crate) struct WsStream {
    config: PathBuf,
    plugin: String,
    receiver: Receiver<Event>,
    _subscription: Subscription,
    head: Vec<u8>,
    head_at: usize,
    /// Server frames waiting for the output buffer.
    frames: VecDeque<PendingFrame>,
    /// Client bytes that arrived before a complete frame.
    buffer: Vec<u8>,
    fragment: Vec<u8>,
    fragment_opcode: Option<u8>,
    /// Payloads that arrived while a call was in flight, in arrival order.
    queued: VecDeque<Vec<u8>>,
    queued_bytes: usize,
    pending_id: Option<Value>,
    call_in_flight: bool,
    heartbeat: Instant,
    /// Bytes of an oversized frame that must be read and dropped before the
    /// close frame is the peer's next read instead of a TCP reset.
    discard: usize,
    close: Option<Vec<u8>>,
    closing: bool,
    /// Counters for the operator log when the transport closes.
    bytes_read: u64,
    frames_read: u64,
}

impl WsStream {
    pub(crate) fn new(
        config: PathBuf,
        plugin: String,
        accept: String,
        receiver: Receiver<Event>,
        subscription: Subscription,
    ) -> WsStream {
        let head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept
        )
        .into_bytes();
        WsStream {
            config,
            plugin,
            receiver,
            _subscription: subscription,
            head,
            head_at: 0,
            frames: VecDeque::new(),
            buffer: Vec::new(),
            fragment: Vec::new(),
            fragment_opcode: None,
            queued: VecDeque::new(),
            queued_bytes: 0,
            pending_id: None,
            call_in_flight: false,
            heartbeat: Instant::now(),
            discard: 0,
            close: None,
            closing: false,
            bytes_read: 0,
            frames_read: 0,
        }
    }

    fn deadline(&self, now: Instant) -> Instant {
        let heartbeat = self.heartbeat + WS_PING;
        let poll = now + POLL;
        let next = heartbeat.min(poll);
        if next > now {
            next
        } else {
            now + Duration::from_millis(1)
        }
    }

    fn step(&self, action: StreamAction, now: Instant) -> StreamStep {
        StreamStep::wait(action).within(self.deadline(now))
    }

    fn queue_frame(&mut self, opcode: u8, payload: Vec<u8>) {
        if self.closing {
            return;
        }
        self.frames.push_back(PendingFrame::new(opcode, payload));
    }

    fn queue_text(&mut self, text: String) {
        if self.closing {
            return;
        }
        self.frames.push_back(PendingFrame::text(text));
    }

    fn queue_close(&mut self, code: u16, reason: &str) {
        self.closing = true;
        self.queued.clear();
        self.queued_bytes = 0;
        if self.close.is_none() {
            self.close = Some(wire::close_payload(code, reason));
        }
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        let now = Instant::now();
        loop {
            if self.head_at < self.head.len() {
                let take = (self.head.len() - self.head_at).min(out.remaining());
                out.write(&self.head[self.head_at..self.head_at + take]);
                self.head_at += take;
                if self.head_at < self.head.len() {
                    return self.step(StreamAction::NeedWrite, now);
                }
            }
            loop {
                let complete = match self.frames.front_mut() {
                    Some(frame) => frame.write(out),
                    None => break,
                };
                if !complete {
                    return self.step(StreamAction::NeedWrite, now);
                }
                self.frames.pop_front();
            }
            if self.closing {
                if let Some(payload) = self.close.take() {
                    let mut frame = PendingFrame::new(0x8, payload);
                    if !frame.write(out) {
                        self.frames.push_front(frame);
                        return self.step(StreamAction::NeedWrite, now);
                    }
                }
                if self.discard > 0 {
                    // The peer is still writing an oversized frame. Keep
                    // reading and dropping it so the close frame is the next
                    // thing it sees, not a TCP reset from our close.
                    return self.step(StreamAction::NeedRead, now);
                }
                if self.frames.is_empty() {
                    return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
                }
            }
            // Do not move a bus event into handler memory unless the socket
            // buffer has room to accept it; the bounded bus channel is the
            // backpressure point when the client is slow.
            if out.remaining() == 0 {
                return self.step(StreamAction::NeedWrite, now);
            }
            match self.receiver.try_recv() {
                Ok(event) => {
                    self.frames.push_back(PendingFrame::text(event.json));
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.queue_close(1000, "");
                    continue;
                }
            }
            if self.frames.is_empty() && self.heartbeat.elapsed() >= WS_PING {
                self.heartbeat = now;
                self.frames.push_back(PendingFrame::new(0x9, Vec::new()));
                continue;
            }
            if !self.closing && !self.call_in_flight && self.frames.is_empty() {
                if let Some(payload) = self.queued.pop_front() {
                    self.queued_bytes = self.queued_bytes.saturating_sub(payload.len());
                    if let Some(job) = self.start_call(&payload) {
                        return self.step(StreamAction::SubmitBlocking(job), now);
                    }
                    continue;
                }
            }
            break;
        }
        if self.frames.is_empty() {
            // The fallback only reads the client when the handler asks for
            // bytes; the Unix loop watches POLLIN regardless. Asking here is
            // harmless on Unix and makes the Windows path full-duplex.
            self.step(StreamAction::NeedRead, now)
        } else {
            self.step(StreamAction::NeedWrite, now)
        }
    }

    fn start_call(&mut self, payload: &[u8]) -> Option<BlockingJob> {
        match webui::bridge_request(&self.plugin, payload) {
            webui::Bridge::Reject(response) => {
                self.queue_text(rpc::to_line(&response));
                None
            }
            webui::Bridge::Forward {
                id,
                name,
                arguments,
            } => {
                self.pending_id = id.clone();
                self.call_in_flight = true;
                let config = self.config.clone();
                Some(Box::new(move |_budget| {
                    match channel::request(
                        &config,
                        "registry.call",
                        serde_json::json!({ "name": name, "arguments": arguments }),
                    ) {
                        Ok(mut response) => {
                            response.id = id;
                            BlockingOutcome::Ok(rpc::to_line(&response).into_bytes())
                        }
                        Err(cause) => {
                            BlockingOutcome::Failed(format!("svcd unreachable: {}", cause))
                        }
                    }
                }))
            }
        }
    }

    fn on_data(&mut self, data: &[u8]) -> Option<BlockingJob> {
        self.bytes_read += data.len() as u64;
        if self.discard > 0 {
            let take = data.len().min(self.discard);
            self.discard -= take;
            return None;
        }
        self.buffer.extend_from_slice(data);
        self.parse()
    }

    fn parse(&mut self) -> Option<BlockingJob> {
        let limits = crate::limits::current();
        let frame_limit = limits.ws_max_message_bytes.max(limits.ws_raw_max_bytes);
        let mut job = None;
        loop {
            match wire::parse_frame(&self.buffer, frame_limit) {
                Ok(None) => break,
                Ok(Some((frame, used))) => {
                    self.buffer.drain(..used);
                    self.frames_read += 1;
                    if self.buffer.is_empty() {
                        let shrink = limits.ws_buffer_shrink_bytes;
                        if self.buffer.capacity() > shrink {
                            self.buffer.shrink_to(shrink);
                        }
                    }
                    // Keep parsing the rest of this read. A second text frame
                    // is queued behind the first call, and control frames must
                    // still be answered while a call is in flight.
                    if let Some(next) = self.handle_frame(&frame) {
                        if job.is_some() {
                            self.queue_close(1013, "too many in-flight requests");
                            break;
                        }
                        job = Some(next);
                    }
                    if self.closing {
                        break;
                    }
                }
                Err(wire::ParseError::TooLarge { total }) => {
                    let buffered = self.buffer.len();
                    self.buffer.clear();
                    self.discard = total.saturating_sub(buffered);
                    self.queue_close(1009, "message too large");
                    break;
                }
                Err(wire::ParseError::Protocol(reason)) => {
                    self.queue_close(1002, reason);
                    break;
                }
            }
        }
        job
    }

    fn handle_frame(&mut self, frame: &wire::Frame) -> Option<BlockingJob> {
        if frame.opcode >= 0x8 {
            match frame.opcode {
                0x8 => {
                    let code = if frame.payload.len() >= 2 {
                        u16::from_be_bytes([frame.payload[0], frame.payload[1]])
                    } else {
                        1000
                    };
                    self.queue_close(code, "");
                }
                0x9 => self.queue_frame(0xA, frame.payload.clone()),
                0xA => {}
                _ => self.queue_close(1002, "unknown control frame"),
            }
            return None;
        }
        let limits = crate::limits::current();
        match frame.opcode {
            0x1 | 0x2 => {
                if self.fragment_opcode.is_some() {
                    self.queue_close(1002, "interleaved data frame");
                    return None;
                }
                if !frame.fin {
                    self.fragment = frame.payload.clone();
                    self.fragment_opcode = Some(frame.opcode);
                    return None;
                }
                self.complete_data(frame.opcode, frame.payload.clone())
            }
            0x0 => {
                let Some(opcode) = self.fragment_opcode else {
                    self.queue_close(1002, "unexpected continuation");
                    return None;
                };
                self.fragment.extend_from_slice(&frame.payload);
                let limit = if opcode == 0x2 {
                    limits.ws_raw_max_bytes
                } else {
                    limits.ws_max_message_bytes
                };
                if self.fragment.len() > limit {
                    self.fragment.clear();
                    self.fragment_opcode = None;
                    self.queue_close(1009, "message too large");
                    return None;
                }
                if frame.fin {
                    let payload = std::mem::take(&mut self.fragment);
                    self.fragment_opcode = None;
                    self.complete_data(opcode, payload)
                } else {
                    None
                }
            }
            _ => {
                self.queue_close(1002, "unknown data frame");
                None
            }
        }
    }

    fn complete_data(&mut self, opcode: u8, payload: Vec<u8>) -> Option<BlockingJob> {
        let limits = crate::limits::current();
        if opcode == 0x2 {
            if !limits.ws_raw_enabled {
                self.queue_close(1003, "binary frames are disabled");
            } else if payload.len() > limits.ws_raw_max_bytes {
                self.queue_close(1009, "raw message too large");
            } else {
                // The old page transport echoed a binary frame back; that is a
                // protocol probe, not a plugin call.
                self.queue_frame(0x2, payload);
            }
            return None;
        }
        if payload.len() > limits.ws_max_message_bytes {
            self.queue_close(1009, "message too large");
            return None;
        }
        if self.call_in_flight {
            if self.queued.len() >= limits.ws_inflight_requests
                || self.queued_bytes.saturating_add(payload.len()) > limits.ws_inflight_bytes
            {
                self.queue_close(1013, "too many in-flight requests");
            } else {
                self.queued_bytes = self.queued_bytes.saturating_add(payload.len());
                self.queued.push_back(payload);
            }
            return None;
        }
        self.start_call(&payload)
    }

    /// Record the response frame. The caller then runs the writable path, which
    /// flushes it before it starts the next queued call: a client must see the
    /// first response before the second call is submitted.
    fn on_blocking_done(&mut self, outcome: BlockingOutcome) {
        self.call_in_flight = false;
        if self.closing {
            self.pending_id = None;
            return;
        }
        match outcome {
            BlockingOutcome::Ok(bytes) => {
                if let Ok(text) = String::from_utf8(bytes) {
                    self.queue_text(text);
                } else {
                    let response = rpc::err(
                        self.pending_id.take(),
                        -32603,
                        "invalid response from management channel",
                    );
                    self.queue_text(rpc::to_line(&response));
                }
            }
            BlockingOutcome::Failed(message) => {
                let response = rpc::err(self.pending_id.take(), -32603, &message);
                self.queue_text(rpc::to_line(&response));
            }
        }
        self.pending_id = None;
    }

    fn log_close(&self, reason: &CloseReason) {
        if matches!(reason, CloseReason::Completed | CloseReason::PeerClosed) {
            return;
        }
        crate::log::svcd(&format!(
            "svcd: plugin ui ws closed: plugin={} reason={:?} bytes={} frames={}",
            self.plugin, reason, self.bytes_read, self.frames_read
        ));
    }
}

impl StreamHandler for WsStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Data(data) => {
                let job = self.on_data(data);
                let step = self.on_writable(out);
                if self.closing {
                    return step;
                }
                match job {
                    Some(job) => self.step(StreamAction::SubmitBlocking(job), Instant::now()),
                    None => step,
                }
            }
            StreamEvent::Tick(_) => self.on_writable(out),
            StreamEvent::BlockingDone(outcome) => {
                self.on_blocking_done(outcome);
                self.on_writable(out)
            }
            StreamEvent::Closed(reason) => {
                self.log_close(&reason);
                StreamStep::wait(StreamAction::Close(reason))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uichannel::EventBus;
    use serde_json::json;
    use std::sync::Arc;

    fn harness() -> (Arc<EventBus>, WsStream) {
        let bus = Arc::new(EventBus::new());
        let (receiver, subscription) = bus.subscribe("demo").expect("subscribe");
        let stream = WsStream::new(
            PathBuf::from("."),
            "demo".to_string(),
            "accept-key".to_string(),
            receiver,
            subscription,
        );
        (bus, stream)
    }

    fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [1u8, 2, 3, 4];
        let mut bytes = wire::frame_header(opcode, payload.len());
        bytes[1] |= 0x80;
        bytes.extend_from_slice(&mask);
        bytes.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        bytes
    }

    fn drive(stream: &mut WsStream, limit: usize) -> Vec<u8> {
        let mut out = Vec::new();
        stream.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
        out
    }

    /// Decode the 101 head, then the server frames in the body.
    fn server_frames(bytes: &[u8]) -> (String, Vec<(u8, Vec<u8>)>) {
        let head_end = bytes.windows(4).position(|window| window == b"\r\n\r\n");
        let head = head_end
            .map(|end| String::from_utf8_lossy(&bytes[..end + 4]).to_string())
            .unwrap_or_default();
        let mut body = head_end.map(|end| &bytes[end + 4..]).unwrap_or(bytes);
        let mut frames = Vec::new();
        while body.len() >= 2 {
            let opcode = body[0] & 0x0F;
            let mut length = u64::from(body[1] & 0x7F);
            let mut at = 2;
            if length == 126 {
                length = u64::from(u16::from_be_bytes([body[at], body[at + 1]]));
                at += 2;
            } else if length == 127 {
                let mut extended = [0u8; 8];
                extended.copy_from_slice(&body[at..at + 8]);
                length = u64::from_be_bytes(extended);
                at += 8;
            }
            let payload = body[at..at + length as usize].to_vec();
            frames.push((opcode, payload));
            body = &body[at + length as usize..];
        }
        (head, frames)
    }

    #[test]
    fn the_handshake_head_is_written_once_before_frames() {
        let (_bus, mut stream) = harness();
        let bytes = drive(&mut stream, 4096);
        let (head, frames) = server_frames(&bytes);
        assert!(
            head.starts_with("HTTP/1.1 101 Switching Protocols"),
            "{head}"
        );
        assert!(
            head.contains("Sec-WebSocket-Accept: accept-key\r\n"),
            "{head}"
        );
        assert!(frames.is_empty());
    }

    #[test]
    fn a_notification_becomes_one_text_frame_and_split_write_keeps_bytes() {
        let (bus, mut stream) = harness();
        let bytes = drive(&mut stream, 4096);
        let (_head, frames) = server_frames(&bytes);
        assert!(frames.is_empty());
        bus.publish("demo", "notifications/message", &json!({"data": "hello"}));
        let mut all = Vec::new();
        for _ in 0..40 {
            all.extend(drive(&mut stream, 12));
        }
        let (_head, frames) = server_frames(&all);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0].0, 0x1);
        assert!(String::from_utf8_lossy(&frames[0].1).contains("hello"));
    }

    #[test]
    fn a_ping_is_answered_with_a_pong() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&client_frame(0x9, b"ping")),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames, vec![(0xA, b"ping".to_vec())]);
    }

    #[test]
    fn split_and_fragmented_text_is_reassembled_before_the_bridge() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        let payload = br#"{"jsonrpc":"2.0","id":1,"method":"nope"}"#;
        let first = &payload[..10];
        let second = &payload[10..];
        let mut frame = client_frame(0x1, first);
        frame[0] &= 0x7F; // fin = false
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&frame),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let continuation = client_frame(0x0, second);
        stream.on_event(
            StreamEvent::Data(&continuation),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames.len(), 1);
        let response: Value = serde_json::from_slice(&frames[0].1).expect("json");
        assert_eq!(response["id"], 1);
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn a_valid_call_submits_one_blocking_job_and_resumes_with_its_result() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        let payload =
            br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"status"}}"#;
        let mut out = Vec::new();
        let step = stream.on_event(
            StreamEvent::Data(&client_frame(0x1, payload)),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let StreamAction::SubmitBlocking(job) = step.action else {
            panic!("a forwarded call is a blocking job");
        };
        let outcome = job(crate::server::http::JobBudget::for_test());
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::BlockingDone(outcome),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames.len(), 1);
        let response: Value = serde_json::from_slice(&frames[0].1).expect("json");
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32603);
    }

    #[test]
    fn a_full_blocking_queue_becomes_a_jsonrpc_error_on_the_page() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        stream.call_in_flight = true;
        stream.pending_id = Some(json!(7));
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::BlockingDone(BlockingOutcome::Failed("the blocking queue is full".into())),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames.len(), 1);
        let response: Value = serde_json::from_slice(&frames[0].1).expect("json");
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32603);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("blocking queue is full"),
            "{response}"
        );
    }

    #[test]
    fn coalesced_calls_are_all_parsed_with_the_first_submitted() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        let first = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status"}}"#;
        let second =
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status"}}"#;
        let mut both = client_frame(0x1, first);
        both.extend(client_frame(0x1, second));
        let mut out = Vec::new();
        let step = stream.on_event(
            StreamEvent::Data(&both),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(step.action, StreamAction::SubmitBlocking(_)));
        assert!(
            stream.buffer.is_empty(),
            "the whole read must be parsed before the driver awaits the job"
        );
        assert_eq!(stream.queued.len(), 1, "the second call is queued");
        assert!(stream.call_in_flight);
    }

    #[test]
    fn a_message_over_the_limit_closes_with_1009() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        let limit = crate::limits::current().ws_max_message_bytes;
        let payload = vec![b'x'; limit + 1];
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&client_frame(0x1, &payload)),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames[0].0, 0x8);
        assert_eq!(u16::from_be_bytes([frames[0].1[0], frames[0].1[1]]), 1009);
    }

    #[test]
    fn a_response_is_written_before_the_next_queued_call_is_submitted() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        stream.call_in_flight = true;
        stream.pending_id = Some(json!(1));
        stream.queued.push_back(
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status"}}"#
                .to_vec(),
        );
        stream.queued_bytes = stream.queued[0].len();
        let mut out = Vec::new();
        let step = stream.on_event(
            StreamEvent::BlockingDone(BlockingOutcome::Ok(
                br#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#.to_vec(),
            )),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames.len(), 1, "the first response is on the wire");
        let response: Value = serde_json::from_slice(&frames[0].1).expect("json");
        assert_eq!(response["id"], 1);
        assert!(
            matches!(step.action, StreamAction::SubmitBlocking(_)),
            "only then is the queued call submitted"
        );
    }

    #[test]
    fn too_many_in_flight_calls_close_with_1013() {
        let (_bus, mut stream) = harness();
        let _ = drive(&mut stream, 4096);
        stream.call_in_flight = true;
        let limits = crate::limits::current();
        for _ in 0..=limits.ws_inflight_requests {
            stream.queued.push_back(vec![b'x'; 1]);
        }
        stream.queued_bytes = limits.ws_inflight_bytes + 1;
        let payload =
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"status"}}"#;
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&client_frame(0x1, payload)),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames[0].0, 0x8);
        assert_eq!(u16::from_be_bytes([frames[0].1[0], frames[0].1[1]]), 1013);
    }
}
