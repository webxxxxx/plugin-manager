//! The stdout protocol channel of one slot: the bounded frame queue a plugin's
//! reader fills, the negotiated protocol version, and whether stdout must stay
//! protocol-only.
//!
//! `Slot` owns process lifetime; this type owns the channel that carries the
//! JSON-RPC frames, so the queue and the version cannot drift apart.

use std::sync::mpsc::Receiver;

use crate::wire::message::ipc::FrameEvent;

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
