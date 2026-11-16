//! Sans-IO stream handlers: events in, actions out.
//!
//! The server owns the connection, the buffers, the deadlines and the
//! blocking pool. A handler owns only protocol state: it never reads or writes
//! a socket, never spawns a thread and never allocates an unbounded output
//! buffer. This is the second half of the S4 seam (ADR-0016).
//!
//! A handler that blocks is a bug. Tests drive a handler with a recorded
//! event sequence under a watchdog; production does not add a timer thread to
//! detect one.

use std::fs::File;
use std::time::Instant;

/// One thing that happened to a stream.
///
/// Events are values, not callbacks: a test can construct any sequence without
/// a socket, a thread or a real clock.
#[derive(Debug)]
pub enum StreamEvent<'a> {
    /// Connection or request-body bytes. A caller may split them anywhere.
    #[allow(dead_code)] // #121 delivers upload bodies as data events.
    Data(&'a [u8]),
    /// The server drained the output buffer and can accept more.
    Writable,
    /// The server's deadline or poll tick fired.
    #[allow(dead_code)] // handlers that only close on a tick ignore the instant.
    Tick(Instant),
    /// A blocking job submitted by this handler finished.
    #[allow(dead_code)] // #116/#118 resume through this event.
    BlockingDone(BlockingOutcome),
    /// The connection is gone. The handler must release its resources; no
    /// action can be delivered.
    #[cfg_attr(not(unix), allow(dead_code))] // the blocking fallback just drops.
    Closed(CloseReason),
}

/// What the server should do after a handler observed an event.
///
/// A step carries one action. Multi-step work (write, then send a file) is
/// expressed as consecutive events: the server delivers `Writable` again once
/// the previous output is on the wire.
pub enum StreamAction {
    /// Nothing pending; wait for the next event the server already watches.
    Continue,
    /// The handler wants body bytes; keep reading the connection.
    #[allow(dead_code)] // #121 upload handlers ask for body bytes.
    NeedRead,
    /// Output is pending; let the socket drain before asking again.
    NeedWrite,
    /// Run this on the blocking pool; resume through `BlockingDone`.
    /// The S4 slice after this one wires the pool (#116).
    #[allow(dead_code)] // #116 submits the first blocking job.
    SubmitBlocking(BlockingJob),
    /// Send this file range on the server's socket once the output buffer
    /// drains. The handler gives up the file handle; the server performs the
    /// send and then delivers `Writable` again, so a multi-part response can
    /// ask for the next range and a finished response returns `Close`.
    SendFile(FileSend),
    /// Finish the response and close the connection.
    #[cfg_attr(not(unix), allow(dead_code))] // the fallback ignores the reason.
    Close(CloseReason),
}

/// Work the handler wants the blocking pool to run. The pool returns the value
/// through `StreamEvent::BlockingDone`; no connection handle is involved.
#[allow(dead_code)] // #116 submits the first blocking job.
pub type BlockingJob = Box<dyn FnOnce() -> BlockingOutcome + Send + 'static>;

/// The value a blocking job returns.
#[allow(dead_code)] // #116 submits the first blocking job.
#[derive(Debug)]
pub enum BlockingOutcome {
    Ok(Vec<u8>),
    Failed(String),
}

/// A file transfer the server performs. The handler owns the handle; the server
/// owns the socket, so neither side shares a descriptor.
pub struct FileSend {
    pub file: File,
    pub offset: u64,
    pub length: u64,
}

/// Why a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))] // some reasons are Unix-loop-only.
pub enum CloseReason {
    /// The response completed normally.
    Completed,
    /// The client went away before the response completed.
    PeerClosed,
    /// The transfer made no progress within its deadline.
    TimedOut,
    /// The handler could not continue. The server logs the message.
    Failed(String),
}

/// What a handler wants next, plus an optional protocol deadline.
pub struct StreamStep {
    pub action: StreamAction,
    /// A deadline shorter than the server's transfer deadline. `None` keeps
    /// the server bound.
    pub deadline: Option<Instant>,
}

impl StreamStep {
    pub fn wait(action: StreamAction) -> StreamStep {
        StreamStep {
            action,
            deadline: None,
        }
    }

    /// Ask the server for an earlier wakeup than its transfer deadline.
    #[allow(dead_code)] // #122 SSE heartbeats ask for a shorter deadline.
    pub fn within(mut self, deadline: Instant) -> StreamStep {
        self.deadline = Some(deadline);
        self
    }
}

/// How `max_streams` accounts for this handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamClass {
    /// A bounded response such as a static asset. It must not consume
    /// `max_streams`, or one browser page would exhaust the long-lived cap.
    OneShot,
    /// A long-lived or transfer handler (SSE, WebSocket, upload, download)
    /// that is bounded by `max_streams`.
    LongLived,
}

/// A sans-IO state machine.
pub trait StreamHandler: Send {
    /// Consume one event. Response bytes are appended to `out`; the returned
    /// step says what the server should do next.
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep;

    /// The default is conservative: an unclassified handler counts against
    /// `max_streams`.
    #[cfg_attr(not(unix), allow(dead_code))] // the Windows fallback is blocking.
    fn stream_class(&self) -> StreamClass {
        StreamClass::LongLived
    }
}

/// A bounded view over the connection's output buffer.
///
/// The limit is the configured stream buffer. A handler may write less than it
/// asked for and must notice the short write through the return value; it must
/// never replace the buffer with one of its own.
pub struct OutBuffer<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}

impl<'a> OutBuffer<'a> {
    pub fn new(bytes: &'a mut Vec<u8>, limit: usize) -> OutBuffer<'a> {
        OutBuffer { bytes, limit }
    }

    /// Bytes that still fit under the limit.
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.bytes.len())
    }

    #[allow(dead_code)] // handlers may branch on a full buffer explicitly.
    pub fn is_full(&self) -> bool {
        self.remaining() == 0
    }

    /// Append as much as fits. Returns whether all of `data` fit.
    pub fn write(&mut self, data: &[u8]) -> bool {
        let take = data.len().min(self.remaining());
        self.bytes.extend_from_slice(&data[..take]);
        take == data.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_buffer_never_grows_past_its_limit() {
        let mut bytes = Vec::new();
        {
            let mut out = OutBuffer::new(&mut bytes, 8);
            assert_eq!(out.remaining(), 8);
            assert!(!out.write(b"0123456789"), "a short write is reported");
            assert!(out.is_full());
            assert!(!out.write(b"x"), "a full buffer accepts nothing");
        }
        assert_eq!(bytes, b"01234567");
        let mut out = OutBuffer::new(&mut bytes, 10);
        assert!(out.write(b"89"));
        assert_eq!(bytes, b"0123456789");
    }

    #[test]
    fn a_step_can_request_a_shorter_deadline() {
        let deadline = Instant::now();
        let step = StreamStep::wait(StreamAction::NeedRead).within(deadline);
        assert_eq!(step.deadline, Some(deadline));
        assert!(matches!(step.action, StreamAction::NeedRead));
    }

    #[test]
    fn an_unclassified_handler_counts_as_long_lived() {
        struct Bare;
        impl StreamHandler for Bare {
            fn on_event(
                &mut self,
                _event: StreamEvent<'_>,
                _out: &mut OutBuffer<'_>,
            ) -> StreamStep {
                StreamStep::wait(StreamAction::Continue)
            }
        }
        struct OneShot;
        impl StreamHandler for OneShot {
            fn on_event(
                &mut self,
                _event: StreamEvent<'_>,
                _out: &mut OutBuffer<'_>,
            ) -> StreamStep {
                StreamStep::wait(StreamAction::Continue)
            }
            fn stream_class(&self) -> StreamClass {
                StreamClass::OneShot
            }
        }
        assert_eq!(Bare.stream_class(), StreamClass::LongLived);
        assert_eq!(OneShot.stream_class(), StreamClass::OneShot);
    }
}
