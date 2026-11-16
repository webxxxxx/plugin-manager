//! Server-side driver for a sans-IO stream handler.
//!
//! The handler owns protocol state; this driver owns the connection's output
//! buffer, the no-progress deadline, the `max_streams` slot and the
//! server-side file send. It never exposes the socket to the handler.

use std::io::{Read, Seek, SeekFrom};
use std::net::TcpStream;
use std::time::Instant;

use crate::limits;
use crate::log;

use super::budget::{Busy, StreamBudget, StreamPermit};
use super::stream::{
    BlockingJob, BlockingOutcome, CloseReason, FileSend, OutBuffer, StreamAction, StreamEvent,
    StreamHandler, StreamStep,
};

pub enum StreamAdmission {
    Admitted(StreamDriver),
    Busy,
}

/// One attached handler plus everything the connection needs to drive it.
pub struct StreamDriver {
    handler: Box<dyn StreamHandler>,
    /// The server's no-progress upper bound.
    deadline: Instant,
    /// The next tick, which a handler may move earlier (poll for bus events,
    /// heartbeats). It can never move past `deadline`.
    wakeup: Instant,
    /// A server-side file send requested by the handler, after the output
    /// buffer drains.
    file: Option<FileSend>,
    /// The handler asked to finish; flush the output buffer, then close.
    closing: Option<CloseReason>,
    /// A blocking job the handler submitted, waiting for the server loop to
    /// accept it. While it is set, or while it runs, the handler gets no other
    /// event: the connection is not making protocol progress.
    job: Option<BlockingJob>,
    /// The server loop took the job; resume on BlockingDone.
    awaiting: bool,
    /// The handler still has output queued, so the server must not read more
    /// client bytes until the socket drains. This is the per-connection
    /// backpressure point for a slow reader.
    blocked_on_write: bool,
    /// Held for long-lived streams, absent for one-shot responses.
    _permit: Option<StreamPermit>,
}

impl StreamDriver {
    /// Admit a handler under `max_streams`. A one-shot response is exempt.
    pub fn admit(handler: Box<dyn StreamHandler>, max_streams: usize) -> StreamAdmission {
        let class = handler.stream_class();
        let permit = match StreamBudget::global().acquire(class, max_streams) {
            Ok(permit) => permit,
            Err(Busy) => return StreamAdmission::Busy,
        };
        let deadline = Instant::now() + limits::current().transfer_timeout;
        StreamAdmission::Admitted(StreamDriver {
            handler,
            deadline,
            wakeup: deadline,
            file: None,
            closing: None,
            job: None,
            awaiting: false,
            blocked_on_write: false,
            _permit: permit,
        })
    }

    pub fn has_file(&self) -> bool {
        self.file.is_some()
    }

    /// Whether the server may read more client bytes. A handler that asked to
    /// write first, or that has a job in flight, must not see new input yet.
    pub fn wants_read(&self) -> bool {
        !self.blocked_on_write && self.job.is_none() && !self.awaiting
    }

    /// The handler asked the server to run a job. The server takes it exactly
    /// once and calls on_blocking_done when it finishes.
    pub fn take_job(&mut self) -> Option<BlockingJob> {
        let job = self.job.take();
        if job.is_some() {
            self.awaiting = true;
        }
        job
    }

    /// Deliver the outcome of the one in-flight job. A duplicate outcome after
    /// a disconnect is ignored.
    pub fn on_blocking_done(
        &mut self,
        outcome: BlockingOutcome,
        out: &mut Vec<u8>,
        out_at: &mut usize,
    ) -> bool {
        if !self.awaiting {
            return true;
        }
        self.awaiting = false;
        self.on_event(StreamEvent::BlockingDone(outcome), out, out_at)
    }

    /// Emit the first response bytes. The engine only calls `on_writable`
    /// after a poll event, so the head would otherwise wait for the next tick.
    pub fn prime(
        &mut self,
        stream: &mut TcpStream,
        out: &mut Vec<u8>,
        out_at: &mut usize,
        last_active: &mut Instant,
    ) -> bool {
        self.on_writable(stream, out, out_at, last_active)
    }

    /// Deliver request-body bytes that were already read with the head.
    pub fn on_data(&mut self, data: &[u8], out: &mut Vec<u8>, out_at: &mut usize) -> bool {
        if data.is_empty() {
            return true;
        }
        if self.job.is_some() || self.awaiting {
            return true;
        }
        self.touch();
        self.on_event(StreamEvent::Data(data), out, out_at)
    }

    pub fn on_readable(
        &mut self,
        stream: &mut TcpStream,
        out: &mut Vec<u8>,
        out_at: &mut usize,
        last_active: &mut Instant,
    ) -> bool {
        if !self.wants_read() || *out_at < out.len() {
            return true;
        }
        let mut chunk = [0u8; 8 * 1024];
        match stream.read(&mut chunk) {
            Ok(0) => false,
            Ok(read) => {
                self.touch();
                *last_active = Instant::now();
                self.on_event(StreamEvent::Data(&chunk[..read]), out, out_at)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }

    /// Flush, then either send a queued file or let the handler write the next
    /// chunk. Returns false when the connection should be removed.
    pub fn on_writable(
        &mut self,
        stream: &mut TcpStream,
        out: &mut Vec<u8>,
        out_at: &mut usize,
        last_active: &mut Instant,
    ) -> bool {
        loop {
            let before = *out_at;
            if !flush(stream, out, out_at, last_active) {
                return false;
            }
            if *out_at > before {
                self.touch();
            }
            if *out_at < out.len() {
                return true;
            }
            if !out.is_empty() {
                out.clear();
            }
            *out_at = 0;
            if self.file.is_some() {
                if !self.pump_file(stream, out) {
                    return false;
                }
                if *out_at < out.len() || self.file.is_some() {
                    return true;
                }
                continue;
            }
            if self.closing.is_some() {
                return false;
            }
            if self.job.is_some() || self.awaiting {
                return true;
            }
            if !self.on_event(StreamEvent::Writable, out, out_at) {
                return false;
            }
            return true;
        }
    }

    /// Deliver a deadline event. The server keeps its own upper bound: a
    /// handler that neither made progress nor asked for a later wakeup closes.
    pub fn on_tick(
        &mut self,
        now: Instant,
        stream: &mut TcpStream,
        out: &mut Vec<u8>,
        out_at: &mut usize,
        last_active: &mut Instant,
    ) -> bool {
        if self.job.is_some() || self.awaiting {
            return true;
        }
        if now < self.wakeup {
            return true;
        }
        if !self.on_event(StreamEvent::Tick(now), out, out_at) {
            return false;
        }
        if self.job.is_some() || self.awaiting {
            return true;
        }
        if now < self.deadline && self.wakeup > now {
            return true;
        }
        self.closing = Some(CloseReason::TimedOut);
        // Flush whatever the handler wrote, then close.
        let _ = flush(stream, out, out_at, last_active);
        false
    }

    fn touch(&mut self) {
        self.deadline = Instant::now() + limits::current().transfer_timeout;
        self.wakeup = self.deadline;
    }

    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut Vec<u8>, out_at: &mut usize) -> bool {
        let limit = limits::current().stream_buffer;
        let before = out.len();
        let step = self
            .handler
            .on_event(event, &mut OutBuffer::new(out, limit));
        let wrote = out.len() > before;
        self.apply(step, wrote, out_at, out.len())
    }

    fn apply(&mut self, step: StreamStep, wrote: bool, out_at: &usize, out_len: usize) -> bool {
        if wrote {
            self.touch();
        }
        if let Some(requested) = step.deadline {
            self.wakeup = requested.min(self.deadline);
            let now = Instant::now();
            if self.wakeup < now {
                self.wakeup = now;
            }
        }
        match step.action {
            StreamAction::Continue | StreamAction::NeedRead => {
                self.blocked_on_write = false;
            }
            StreamAction::NeedWrite => {
                self.blocked_on_write = true;
            }
            StreamAction::SubmitBlocking(job) => {
                self.blocked_on_write = false;
                if self.job.is_some() || self.awaiting {
                    self.closing = Some(CloseReason::Failed(
                        "a stream handler submitted a second blocking job".into(),
                    ));
                } else {
                    self.job = Some(job);
                }
            }
            StreamAction::SendFile(send) => {
                self.blocked_on_write = false;
                self.file = Some(send);
                // A file send is the last thing this response does.
                self.closing = Some(CloseReason::Completed);
            }
            StreamAction::Close(reason) => {
                self.blocked_on_write = false;
                self.closing = Some(reason);
            }
        }
        !(self.closing.is_some() && self.file.is_none() && *out_at >= out_len)
    }

    /// Advance the server-side file send. One-shot assets keep the zero-copy
    /// kernel path where the platform has an incremental one; elsewhere the
    /// bounded read+write fallback keeps memory inside the stream buffer.
    fn pump_file(&mut self, stream: &mut TcpStream, out: &mut Vec<u8>) -> bool {
        let limit = limits::current().stream_buffer;
        loop {
            let Some(mut send) = self.file.take() else {
                return true;
            };
            if send.length == 0 {
                continue;
            }
            match crate::zerocopy::try_send_file_range(
                &mut send.file,
                stream,
                send.offset,
                send.length,
            ) {
                Ok(crate::zerocopy::FileSendProgress::Sent(sent)) => {
                    send.offset += sent;
                    send.length -= sent;
                    self.file = Some(send);
                    self.touch();
                    continue;
                }
                Ok(crate::zerocopy::FileSendProgress::Done) => {
                    self.touch();
                    continue;
                }
                Ok(crate::zerocopy::FileSendProgress::WouldBlock) => {
                    self.file = Some(send);
                    return true;
                }
                Ok(crate::zerocopy::FileSendProgress::Unsupported) => {
                    match read_file_chunk(&mut send, out, limit) {
                        Ok(0) => {
                            // The output buffer is full; flush it before reading.
                            self.file = Some(send);
                            return true;
                        }
                        Ok(_) => {
                            self.file = Some(send);
                            self.touch();
                            return true;
                        }
                        Err(error) => {
                            log::svcd(&format!("svcd: plugin UI asset read failed: {}", error));
                            self.closing =
                                Some(CloseReason::Failed(format!("read UI asset: {}", error)));
                            return true;
                        }
                    }
                }
                Err(error) => {
                    log::svcd(&format!("svcd: plugin UI file send failed: {}", error));
                    self.closing = Some(CloseReason::Failed(format!("send UI asset: {}", error)));
                    return true;
                }
            }
        }
    }
}

impl Drop for StreamDriver {
    fn drop(&mut self) {
        let reason = self.closing.clone().unwrap_or(CloseReason::PeerClosed);
        let mut discarded = Vec::new();
        let _ = self.handler.on_event(
            StreamEvent::Closed(reason),
            &mut OutBuffer::new(&mut discarded, 0),
        );
    }
}

fn flush(
    stream: &mut TcpStream,
    out: &mut Vec<u8>,
    out_at: &mut usize,
    last_active: &mut Instant,
) -> bool {
    if *out_at >= out.len() {
        out.clear();
        *out_at = 0;
        return true;
    }
    match std::io::Write::write(stream, &out[*out_at..]) {
        Ok(0) => false,
        Ok(written) => {
            *out_at += written;
            *last_active = Instant::now();
            if *out_at >= out.len() {
                out.clear();
                *out_at = 0;
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

/// Read the next bounded chunk of a file into the connection's output buffer.
/// Returns 0 when the buffer is full rather than at EOF.
fn read_file_chunk(send: &mut FileSend, out: &mut Vec<u8>, limit: usize) -> std::io::Result<usize> {
    let capacity = limit.saturating_sub(out.len());
    let want = std::cmp::min(capacity as u64, send.length) as usize;
    if want == 0 {
        return Ok(0);
    }
    send.file.seek(SeekFrom::Start(send.offset))?;
    let base = out.len();
    out.resize(base + want, 0);
    match send.file.read(&mut out[base..]) {
        Ok(0) => {
            out.truncate(base);
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "UI asset ended before its advertised length",
            ))
        }
        Ok(read) => {
            out.truncate(base + read);
            send.offset += read as u64;
            send.length -= read as u64;
            Ok(read)
        }
        Err(error) => {
            out.truncate(base);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct JobHandler;

    impl StreamHandler for JobHandler {
        fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
            match event {
                StreamEvent::Data(_) => {
                    StreamStep::wait(StreamAction::SubmitBlocking(Box::new(|| {
                        BlockingOutcome::Ok(b"done".to_vec())
                    })))
                }
                StreamEvent::BlockingDone(BlockingOutcome::Ok(bytes)) => {
                    out.write(&bytes);
                    StreamStep::wait(StreamAction::Close(CloseReason::Completed))
                }
                StreamEvent::BlockingDone(BlockingOutcome::Failed(message)) => {
                    out.write(message.as_bytes());
                    StreamStep::wait(StreamAction::Close(CloseReason::Failed(message)))
                }
                _ => StreamStep::wait(StreamAction::NeedRead),
            }
        }
    }

    #[test]
    fn a_submitted_job_is_taken_once_and_resumed_with_its_outcome() {
        let StreamAdmission::Admitted(mut driver) = StreamDriver::admit(Box::new(JobHandler), 8)
        else {
            panic!("the stream fits the cap");
        };
        let mut out = Vec::new();
        let mut out_at = 0;
        assert!(driver.on_data(b"request", &mut out, &mut out_at));
        let job = driver.take_job().expect("the handler submitted one job");
        assert!(driver.take_job().is_none(), "a job is taken once");
        assert!(
            driver.on_data(b"more", &mut out, &mut out_at),
            "input is not delivered while the job runs"
        );
        assert!(out.is_empty(), "the handler saw no second event");
        assert!(driver.on_blocking_done(
            BlockingOutcome::Ok(job_result(job)),
            &mut out,
            &mut out_at
        ));
        assert_eq!(out, b"done");
        assert!(
            driver.closing.is_some(),
            "the handler closed after the result"
        );
    }

    fn job_result(job: BlockingJob) -> Vec<u8> {
        match job() {
            BlockingOutcome::Ok(bytes) => bytes,
            BlockingOutcome::Failed(message) => panic!("unexpected failure: {message}"),
        }
    }

    #[test]
    fn a_handler_may_submit_a_follow_up_job_after_a_completion() {
        struct TwoJobs {
            calls: u8,
        }
        impl StreamHandler for TwoJobs {
            fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
                match event {
                    StreamEvent::Data(_) => {
                        StreamStep::wait(StreamAction::SubmitBlocking(Box::new(|| {
                            BlockingOutcome::Ok(b"one".to_vec())
                        })))
                    }
                    StreamEvent::BlockingDone(BlockingOutcome::Ok(bytes)) => {
                        self.calls += 1;
                        out.write(&bytes);
                        if self.calls == 1 {
                            StreamStep::wait(StreamAction::SubmitBlocking(Box::new(|| {
                                BlockingOutcome::Ok(b"two".to_vec())
                            })))
                        } else {
                            StreamStep::wait(StreamAction::Close(CloseReason::Completed))
                        }
                    }
                    StreamEvent::BlockingDone(BlockingOutcome::Failed(message)) => {
                        StreamStep::wait(StreamAction::Close(CloseReason::Failed(message)))
                    }
                    _ => StreamStep::wait(StreamAction::Continue),
                }
            }
        }
        let StreamAdmission::Admitted(mut driver) =
            StreamDriver::admit(Box::new(TwoJobs { calls: 0 }), 8)
        else {
            panic!("the stream fits the cap");
        };
        let mut out = Vec::new();
        let mut out_at = 0;
        driver.on_data(b"x", &mut out, &mut out_at);
        let first = driver.take_job().expect("first job");
        driver.on_blocking_done(
            BlockingOutcome::Ok(job_result(first)),
            &mut out,
            &mut out_at,
        );
        let second = driver
            .take_job()
            .expect("a WebSocket may queue the next call after a response");
        driver.on_blocking_done(
            BlockingOutcome::Ok(job_result(second)),
            &mut out,
            &mut out_at,
        );
        assert_eq!(out, b"onetwo");
        assert!(driver.closing.is_some());
    }
}
