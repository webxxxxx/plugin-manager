//! Protocol-3 raw WebSocket as a sans-IO stream handler.
//!
//! The raw hub owns the plugin side; this handler owns the browser side:
//! binary frames only, one fragmented message at a time, heartbeat pings, and
//! the same close reasons the reactor used. Subscription frames are drained on
//! writable/tick events; an output buffer that stays full is the backpressure
//! point, and the hub's bounded subscriber queue drops a UI that stops reading.

use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::server::http::{
    CloseReason, OutBuffer, StreamAction, StreamEvent, StreamHandler, StreamStep,
};
use crate::wire::contract::rawhub::{RawEvent, RawHub, RawSubscription};
use crate::wire::message::ipc::RawFailure;
use crate::wire::ws::frame;

use super::frame::{write_queued, PendingFrame, QueuedWrite};

const RAW_PING: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);

/// The plugin side of one raw stream. A trait so the handler can be driven
/// without a real plugin process in unit tests.
pub(crate) trait RawLink: Send {
    fn plugin(&self) -> &str;
    fn stream(&self) -> u32;
    fn try_recv(&self) -> Result<RawEvent, TryRecvError>;
    fn send(&self, payload: &[u8]) -> Result<(), String>;
}

impl RawLink for RawSubscription {
    fn plugin(&self) -> &str {
        RawSubscription::plugin(self)
    }

    fn stream(&self) -> u32 {
        RawSubscription::stream(self)
    }

    fn try_recv(&self) -> Result<RawEvent, TryRecvError> {
        RawSubscription::try_recv(self)
    }

    fn send(&self, payload: &[u8]) -> Result<(), String> {
        RawSubscription::send(self, payload)
    }
}

pub(crate) struct RawWsStream {
    link: Box<dyn RawLink>,
    hub: Option<Arc<RawHub>>,
    head: Vec<u8>,
    head_at: usize,
    frames: VecDeque<PendingFrame>,
    buffer: Vec<u8>,
    fragment: Vec<u8>,
    fragment_opcode: Option<u8>,
    heartbeat: Instant,
    discard: usize,
    close: Option<Vec<u8>>,
    closing: bool,
    bytes_read: u64,
    bytes_parsed: u64,
    frames_read: u64,
    limits: std::sync::Arc<crate::platform::limits::Limits>,
}

impl RawWsStream {
    pub(crate) fn new(
        accept: String,
        link: Box<dyn RawLink>,
        hub: Option<Arc<RawHub>>,
        limits: std::sync::Arc<crate::platform::limits::Limits>,
    ) -> RawWsStream {
        let head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept
        )
        .into_bytes();
        RawWsStream {
            link,
            hub,
            head,
            head_at: 0,
            frames: VecDeque::new(),
            buffer: Vec::new(),
            fragment: Vec::new(),
            fragment_opcode: None,
            heartbeat: Instant::now(),
            discard: 0,
            close: None,
            closing: false,
            bytes_read: 0,
            bytes_parsed: 0,
            frames_read: 0,
            limits,
        }
    }

    fn log(&self, reason: &str) {
        info!(
            "raw ws closed: plugin={} stream={} reason={} read={} parsed={} frames={}",
            self.link.plugin(),
            self.link.stream(),
            reason,
            self.bytes_read,
            self.bytes_parsed,
            self.frames_read
        );
    }

    fn deadline(&self, now: Instant) -> Instant {
        let heartbeat = self.heartbeat + RAW_PING;
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

    fn queue_close(&mut self, code: u16, reason: &str) {
        self.closing = true;
        if self.close.is_none() {
            self.close = Some(frame::close_payload(code, reason));
        }
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        let now = Instant::now();
        loop {
            match write_queued(
                &self.head,
                &mut self.head_at,
                &mut self.frames,
                &mut self.close,
                self.closing,
                self.discard,
                out,
            ) {
                QueuedWrite::NeedWrite => return self.step(StreamAction::NeedWrite, now),
                QueuedWrite::NeedRead => return self.step(StreamAction::NeedRead, now),
                QueuedWrite::Closed => {
                    return StreamStep::wait(StreamAction::Close(CloseReason::Completed))
                }
                QueuedWrite::Complete => {}
            }
            // Do not move a hub event into handler memory unless the socket
            // buffer has room to accept it; the hub's bounded subscriber queue
            // is the backpressure point when the client is slow.
            if out.remaining() == 0 {
                return self.step(StreamAction::NeedWrite, now);
            }
            match self.link.try_recv() {
                Ok(RawEvent::Frame(payload)) => {
                    self.frames.push_back(PendingFrame::new(0x2, payload));
                    continue;
                }
                Ok(RawEvent::Closed) => {
                    self.log("raw closed");
                    self.queue_close(1000, "raw closed");
                    continue;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.log("raw channel closed");
                    self.queue_close(1000, "raw closed");
                    continue;
                }
            }
            if self.frames.is_empty() && self.heartbeat.elapsed() >= RAW_PING {
                self.heartbeat = now;
                self.frames.push_back(PendingFrame::new(0x9, Vec::new()));
                continue;
            }
            break;
        }
        if self.frames.is_empty() {
            self.step(StreamAction::NeedRead, now)
        } else {
            self.step(StreamAction::NeedWrite, now)
        }
    }

    fn on_data(&mut self, data: &[u8]) {
        self.bytes_read += data.len() as u64;
        if self.discard > 0 {
            let take = data.len().min(self.discard);
            self.discard -= take;
            return;
        }
        self.buffer.extend_from_slice(data);
        self.parse();
    }

    fn parse(&mut self) {
        let limit = self.limits.ws_raw_max_bytes;
        loop {
            match frame::parse_frame(&self.buffer, limit) {
                Ok(None) => break,
                Ok(Some((frame, used))) => {
                    self.bytes_parsed += used as u64;
                    self.frames_read += 1;
                    self.buffer.drain(..used);
                    if self.buffer.is_empty() {
                        let shrink = self.limits.ws_buffer_shrink_bytes;
                        if self.buffer.capacity() > shrink {
                            self.buffer.shrink_to(shrink);
                        }
                    }
                    self.handle_frame(&frame);
                    if self.closing {
                        break;
                    }
                }
                Err(frame::ParseError::TooLarge { total }) => {
                    let buffered = self.buffer.len();
                    self.buffer.clear();
                    self.discard = total.saturating_sub(buffered);
                    self.queue_close(1009, "message too large");
                    break;
                }
                Err(frame::ParseError::Protocol(reason)) => {
                    let head: Vec<String> = self
                        .buffer
                        .iter()
                        .take(24)
                        .map(|byte| format!("{:02x}", byte))
                        .collect();
                    self.log(&format!(
                        "protocol error: {} buffer={} head={}",
                        reason,
                        self.buffer.len(),
                        head.join(" ")
                    ));
                    self.queue_close(1002, reason);
                    break;
                }
            }
        }
    }

    fn handle_frame(&mut self, frame: &frame::Frame) {
        if frame.opcode >= 0x8 {
            match frame.opcode {
                0x8 => match frame::close_code(&frame.payload) {
                    Ok(code) => self.queue_close(code.unwrap_or(1000), ""),
                    Err(frame::CloseError::InvalidUtf8) => {
                        self.queue_close(1007, "close reason is not utf-8")
                    }
                    Err(frame::CloseError::Protocol(reason)) => self.queue_close(1002, reason),
                },
                0x9 => self.queue_frame(0xA, frame.payload.clone()),
                0xA => {}
                _ => self.queue_close(1002, "unknown control frame"),
            }
            return;
        }
        let limit = self.limits.ws_raw_max_bytes;
        match frame.opcode {
            0x2 => {
                if frame.payload.len() > limit {
                    self.queue_close(1009, "raw message too large");
                    return;
                }
                if self.fragment_opcode.is_some() {
                    self.queue_close(1002, "interleaved data frame");
                    return;
                }
                if !frame.fin {
                    self.fragment = frame.payload.clone();
                    self.fragment_opcode = Some(0x2);
                    return;
                }
                self.send_complete(frame.payload.clone());
            }
            0x1 => {
                self.log("text frame on a binary-only transport");
                self.queue_close(1003, "raw transport accepts binary frames only");
            }
            0x0 => {
                if self.fragment_opcode != Some(0x2) {
                    self.queue_close(1002, "unexpected continuation");
                    return;
                }
                self.fragment.extend_from_slice(&frame.payload);
                if self.fragment.len() > limit {
                    self.fragment.clear();
                    self.fragment_opcode = None;
                    self.queue_close(1009, "raw message too large");
                    return;
                }
                if frame.fin {
                    let payload = std::mem::take(&mut self.fragment);
                    self.fragment_opcode = None;
                    self.send_complete(payload);
                }
            }
            _ => self.queue_close(1002, "unknown raw frame"),
        }
    }

    fn send_complete(&mut self, payload: Vec<u8>) {
        if let Err(reason) = self.link.send(&payload) {
            self.log(&format!("send rejected: {}", reason));
            self.queue_close(1013, &reason);
        }
    }
}

impl StreamHandler for RawWsStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Data(data) => {
                self.on_data(data);
                self.on_writable(out)
            }
            StreamEvent::Tick(_) => self.on_writable(out),
            StreamEvent::BlockingDone(_) => self.on_writable(out),
            StreamEvent::Closed(reason) => {
                match &reason {
                    CloseReason::PeerClosed | CloseReason::Completed => {}
                    CloseReason::TimedOut => self.log("timed out"),
                    CloseReason::Failed(message) => {
                        self.log(message);
                        if let Some(hub) = &self.hub {
                            hub.note_transport_failure(
                                RawFailure::WsWriteError,
                                self.link.plugin(),
                            );
                        }
                    }
                }
                StreamStep::wait(StreamAction::Close(reason))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::ws::frame::server_frames;
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

    struct FakeLink {
        plugin: String,
        stream: u32,
        events: Receiver<RawEvent>,
        sender: SyncSender<Vec<u8>>,
    }

    impl RawLink for FakeLink {
        fn plugin(&self) -> &str {
            &self.plugin
        }

        fn stream(&self) -> u32 {
            self.stream
        }

        fn try_recv(&self) -> Result<RawEvent, TryRecvError> {
            self.events.try_recv()
        }

        fn send(&self, payload: &[u8]) -> Result<(), String> {
            self.sender
                .send(payload.to_vec())
                .map_err(|_| "closed".into())
        }
    }

    fn test_limits() -> Arc<crate::platform::limits::Limits> {
        crate::platform::test_support::limits()
    }

    fn harness() -> (RawWsStream, Receiver<Vec<u8>>, SyncSender<RawEvent>) {
        let (events_tx, events_rx) = sync_channel(4);
        let (sent_tx, sent_rx) = sync_channel(4);
        let link = FakeLink {
            plugin: "echo".into(),
            stream: 1,
            events: events_rx,
            sender: sent_tx,
        };
        (
            RawWsStream::new("accept".into(), Box::new(link), None, test_limits()),
            sent_rx,
            events_tx,
        )
    }

    fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [9u8, 8, 7, 6];
        let mut bytes = frame::frame_header(opcode, payload.len());
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

    use super::super::drive_writable as drive;

    #[test]
    fn a_binary_frame_reaches_the_plugin_byte_for_byte() {
        let (mut stream, sent, _events) = harness();
        let _ = drive(&mut stream, 4096);
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&client_frame(0x2, b"raw\x00\xff")),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert_eq!(sent.recv().expect("sent"), b"raw\x00\xff");
    }

    #[test]
    fn invalid_close_frames_are_rejected_instead_of_echoed() {
        let cases: Vec<(Vec<u8>, u16, &str)> = vec![
            (vec![0x03], 1002, "close frame payload length 1"),
            (1004u16.to_be_bytes().to_vec(), 1002, "invalid close code"),
            (
                {
                    let mut payload = 1000u16.to_be_bytes().to_vec();
                    payload.push(0xFF);
                    payload
                },
                1007,
                "close reason is not utf-8",
            ),
        ];
        for (payload, code, reason) in cases {
            let (mut stream, _sent, _events) = harness();
            let _ = drive(&mut stream, 4096);
            let mut out = Vec::new();
            stream.on_event(
                StreamEvent::Data(&client_frame(0x8, &payload)),
                &mut OutBuffer::new(&mut out, 4096),
            );
            let (_, frames) = server_frames(&out);
            assert_eq!(
                frames,
                vec![(0x8, frame::close_payload(code, reason))],
                "{payload:?}"
            );
        }
    }

    #[test]
    fn a_fragmented_message_is_reassembled_first() {
        let (mut stream, sent, _events) = harness();
        let _ = drive(&mut stream, 4096);
        let mut first = client_frame(0x2, b"part-one/");
        first[0] &= 0x7F;
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&first),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(sent.try_recv().is_err(), "a fragment is not sent yet");
        stream.on_event(
            StreamEvent::Data(&client_frame(0x0, b"part-two")),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert_eq!(sent.recv().expect("sent"), b"part-one/part-two");
    }

    #[test]
    fn a_plugin_frame_becomes_one_binary_server_frame() {
        let (mut stream, _sent, events) = harness();
        let _ = drive(&mut stream, 4096);
        events
            .send(RawEvent::Frame(b"from-plugin".to_vec()))
            .expect("event");
        let bytes = drive(&mut stream, 4096);
        let (_, frames) = server_frames(&bytes);
        assert_eq!(frames, vec![(0x2, b"from-plugin".to_vec())]);
    }

    #[test]
    fn a_text_frame_closes_with_1003() {
        let (mut stream, _sent, _events) = harness();
        let _ = drive(&mut stream, 4096);
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&client_frame(0x1, b"text")),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames[0].0, 0x8);
        assert_eq!(u16::from_be_bytes([frames[0].1[0], frames[0].1[1]]), 1003);
    }

    #[test]
    fn an_oversized_message_closes_with_1009() {
        let (mut stream, _sent, _events) = harness();
        let _ = drive(&mut stream, 4096);
        let limit = test_limits().ws_raw_max_bytes;
        let mut out = Vec::new();
        stream.on_event(
            StreamEvent::Data(&client_frame(0x2, &vec![0x41; limit + 1])),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let (_, frames) = server_frames(&out);
        assert_eq!(frames[0].0, 0x8);
        assert_eq!(u16::from_be_bytes([frames[0].1[0], frames[0].1[1]]), 1009);
    }

    #[test]
    fn a_hub_close_writes_the_1000_close_frame() {
        let (mut stream, _sent, events) = harness();
        let _ = drive(&mut stream, 4096);
        events.send(RawEvent::Closed).expect("event");
        let bytes = drive(&mut stream, 4096);
        let (_, frames) = server_frames(&bytes);
        assert_eq!(frames[0].0, 0x8);
        assert_eq!(u16::from_be_bytes([frames[0].1[0], frames[0].1[1]]), 1000);
    }
}
