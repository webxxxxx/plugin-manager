//! The stdout protocol channel of one slot: the bounded frame queue a plugin's
//! reader fills, the negotiated protocol version, and whether stdout must stay
//! protocol-only.
//!
//! `Slot` owns process lifetime; this type owns the channel that carries the
//! JSON-RPC frames, so the queue and the version cannot drift apart.

use std::sync::mpsc::Receiver;

use crate::wire::message::ipc::{FrameError, FrameEvent};

use super::*;

pub(super) struct Frames {
    /// `None` when no reader is attached (exec-ready, raw transport, stopped).
    rx: Option<Receiver<FrameEvent>>,
    /// JSON-RPC framing version: `>= 2` uses length-prefixed frames.
    protocol: u32,
    /// Plugins must keep stdout protocol-only; core scripts may still print
    /// human-readable output, which is logged for diagnosis.
    protocol_only: bool,
}

impl Frames {
    pub(super) fn new() -> Frames {
        Frames {
            rx: None,
            protocol: 1,
            protocol_only: false,
        }
    }

    pub(super) fn protocol(&self) -> u32 {
        self.protocol
    }

    pub(super) fn set_protocol(&mut self, protocol: u32) {
        self.protocol = protocol;
    }

    pub(super) fn protocol_only(&self) -> bool {
        self.protocol_only
    }

    pub(super) fn set_protocol_only(&mut self, protocol_only: bool) {
        self.protocol_only = protocol_only;
    }

    /// The reader thread handed us the receive end of its bounded queue.
    pub(super) fn attach(&mut self, rx: Receiver<FrameEvent>) {
        self.rx = Some(rx);
    }

    /// Drop the queue: the process is gone or its stdout moved to another path.
    pub(super) fn detach(&mut self) {
        self.rx = None;
    }

    pub(super) fn attached(&self) -> bool {
        self.rx.is_some()
    }

    /// Take every queued event without blocking. Collected first so the
    /// handlers can borrow the rest of the slot mutably while they run.
    pub(super) fn drain(&self) -> Vec<FrameEvent> {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        events
    }
}

impl Slot {
    /// One line of readiness-handshake state for the start-timeout log.
    ///
    /// Read-only: the start wait calls it just before reporting a timeout, so a
    /// failure names which hop stalled instead of only saying "timed out".
    pub(crate) fn handshake_snapshot(&self) -> String {
        format!(
            "state={:?} pid={:?} ping_id={} stdin={} frames={} start_deadline={:?}",
            self.machine.state,
            self.machine.pid,
            self.ping_id,
            if self.stdin.is_some() {
                "open"
            } else {
                "closed"
            },
            if self.frames.attached() {
                "attached"
            } else {
                "none"
            },
            self.start_deadline,
        )
    }

    pub(super) fn write_json_frame(&mut self, frame: &str, max: usize) -> std::io::Result<()> {
        let protocol = self.frames.protocol();
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "plugin stdin is closed",
            ));
        };
        if protocol >= 2 {
            ipc::write_length_frame_limit(stdin, frame, max)
        } else {
            ipc::write_frame_limit(stdin, frame, max)
        }
    }

    pub(super) fn drain_frames(&mut self, now: u64) {
        let events = self.frames.drain();
        for event in events {
            match event {
                FrameEvent::Frame(Ok(line)) => self.handle_frame(&line, now),
                FrameEvent::Frame(Err(FrameError::TooLarge { max })) => {
                    self.protocol_violation(&format!("frame exceeds {} bytes", max));
                    break;
                }
                FrameEvent::Frame(Err(FrameError::InvalidUtf8)) => {
                    self.protocol_violation("frame is not valid UTF-8");
                    break;
                }
                FrameEvent::Frame(Err(FrameError::Io(_))) | FrameEvent::Closed => {
                    self.stdout_closed()
                }
            }
        }
    }

    pub(super) fn handle_frame(&mut self, line: &str, now: u64) {
        let message = match serde_json::from_str::<Value>(line) {
            Ok(Value::Object(message)) => message,
            _ => {
                if self.frames.protocol_only() {
                    self.protocol_violation("stdout frame is not a JSON object");
                } else {
                    // Core scripts may print to stdout; keep those lines in the
                    // service log for diagnosis.
                    self.note_frame_during_start("not a JSON object", line);
                    self.log_service(line);
                }
                return;
            }
        };
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        if let Some(method) = method {
            if id.is_some() {
                self.protocol_violation(
                    "plugin sent a request; only responses and notifications are allowed",
                );
                return;
            }
            self.handle_notification(method, message.get("params"));
            return;
        }
        let Some(id) = id.and_then(Value::as_u64) else {
            if self.frames.protocol_only() {
                self.protocol_violation("stdout frame has no method or response id");
            } else {
                self.note_frame_during_start("no method or response id", line);
                self.log_service(line);
            }
            return;
        };
        if self.machine.state == State::Starting {
            if id == self.ping_id {
                match self.machine.pid {
                    Some(pid) => {
                        log::svcd(&format!(
                            "svcd: {}: ready ping {} answered by pid {}",
                            self.id, id, pid
                        ));
                        self.apply(Event::Ready { pid }, now);
                    }
                    None => log::svcd(&format!(
                        "svcd: {}: ready ping {} answered before the pid was known; dropped",
                        self.id, id
                    )),
                }
                return;
            }
            // Not the ready reply; the pending-call path below keeps its own log.
            log::svcd(&format!(
                "svcd: {}: frame id {} during Starting is not the ready ping {}",
                self.id, id, self.ping_id
            ));
        }
        self.complete_pending(id, &message);
    }

    pub(super) fn handle_notification(&mut self, method: &str, params: Option<&Value>) {
        if method == "notifications/progress" {
            if let Some(token) = params
                .and_then(|params| params.get("progressToken"))
                .and_then(Value::as_str)
            {
                let line = json!({
                    "jsonrpc": "2.0",
                    "method": method,
                    "params": params.cloned().unwrap_or(Value::Null),
                })
                .to_string();
                for pending in self
                    .pending
                    .values()
                    .filter(|pending| pending.progress_token.as_deref() == Some(token))
                {
                    if let Some(client) = &pending.client {
                        client.send(line.clone());
                    }
                }
            }
        }
        let text = match method {
            "notifications/message" => params
                .and_then(|params| params.get("data"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| params.map(Value::to_string).unwrap_or_default()),
            "notifications/cancelled" => format!(
                "cancelled: {}",
                params.map(Value::to_string).unwrap_or_default()
            ),
            other => format!(
                "notification {}: {}",
                other,
                params.map(Value::to_string).unwrap_or_default()
            ),
        };
        self.log_service(&text);
        if let Some(notify) = &self.notify_sink {
            notify(method, &params.cloned().unwrap_or(Value::Null));
        }
    }

    pub(super) fn write_notification(&mut self, method: &str, params: Value) {
        let frame = rpc::notification(method, params);
        let _ = self.write_json_frame(&frame, ipc::CONTROL_FRAME_MAX);
    }

    /// stdout EOF means the plugin can no longer answer. Treat it as an exit:
    /// force the process down and let the supervisor withdraw its capabilities.
    pub(super) fn stdout_closed(&mut self) {
        if matches!(self.machine.state, State::Starting | State::Running) {
            log::svcd(&format!(
                "svcd: {}: stdout closed; treating the plugin as exited",
                self.id
            ));
            self.signal_force("stdout closed");
        }
    }

    pub(super) fn protocol_violation(&mut self, reason: &str) {
        log::svcd(&format!(
            "svcd: {}: protocol violation: {}; killing process",
            self.id, reason
        ));
        self.log_service(&format!("protocol violation: {}", reason));
        self.fail_pending("plugin protocol violation");
        self.signal_force("protocol violation");
    }

    /// Readiness window only: note a stdout line that cannot become Ready.
    ///
    /// Bounded to `Starting` on purpose: the steady-state call path must stay
    /// silent, and this is the only window where a stray line blocks a start.
    pub(super) fn note_frame_during_start(&self, reason: &str, line: &str) {
        if self.machine.state != State::Starting {
            return;
        }
        let excerpt: String = line.chars().take(200).collect();
        log::svcd(&format!(
            "svcd: {}: stdout during Starting is not usable for ready ({}): {}",
            self.id, reason, excerpt
        ));
    }

    pub(super) fn log_service(&self, line: &str) {
        if let Some(sink) = &self.log {
            if let Ok(mut sink) = sink.lock() {
                let _ = sink.line(line);
            }
        }
    }
}
