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
    /// The server's no-progress upper bound. Only real output progress moves
    /// it; a handler-requested wakeup never replaces it.
    deadline: Instant,
    /// The next tick. A handler may move it earlier (poll for upstream
    /// events, heartbeat), but never past `deadline`.
    wakeup: Instant,
    /// A server-side file send requested by the handler, after the output
    /// buffer drains.
    file: Option<FileSend>,
    /// The handler asked to finish; flush the output buffer, then close.
    closing: Option<CloseReason>,
    /// A blocking job the handler wants the server loop to submit.
    pending_job: Option<BlockingJob>,
    /// A job is queued or running on the blocking pool. No socket reads and no
    /// handler events run until its completion arrives.
    blocking: bool,
    /// When the handler last paused for a blocking job. A queued or running
    /// job that makes no progress for the transfer timeout closes the stream.
    paused: Option<Instant>,
    /// Held for long-lived streams, absent for one-shot responses.
    _permit: Option<StreamPermit>,
}

impl StreamDriver {
    /// Admit a handler under `max_streams`. A one-shot response is exempt.
    pub fn admit(handler: Box<dyn StreamHandler>, max_streams: usize) -> StreamAdmission {
        match StreamBudget::global().acquire(handler.stream_class(), max_streams) {
            Ok(permit) => StreamAdmission::Admitted(StreamDriver::new(handler, permit)),
            Err(Busy) => StreamAdmission::Busy,
        }
    }

    /// Admit a handler whose budget slot was reserved before a side effect.
    /// A streaming `tools/call` opens the outbound management call only after
    /// the slot is known to fit, so a full cap refuses the request without
    /// starting the plugin call. The permit is released when the driver drops.
    pub fn admit_reserved(
        handler: Box<dyn StreamHandler>,
        permit: super::budget::StreamPermit,
    ) -> StreamDriver {
        StreamDriver::new(handler, Some(permit))
    }

    fn new(
        handler: Box<dyn StreamHandler>,
        permit: Option<super::budget::StreamPermit>,
    ) -> StreamDriver {
        let deadline = Instant::now() + limits::current().transfer_timeout;
        StreamDriver {
            handler,
            deadline,
            wakeup: deadline,
            file: None,
            closing: None,
            pending_job: None,
            blocking: false,
            paused: None,
            _permit: permit,
        }
    }

    pub fn has_file(&self) -> bool {
        self.file.is_some()
    }

    /// Whether the server may read more request-body bytes. A pending or
    /// running blocking job pauses input; the staged upload therefore applies
    /// backpressure to the socket instead of growing a queue.
    pub fn wants_read(&self) -> bool {
        self.pending_job.is_none() && !self.blocking
    }

    /// Take the job the handler just submitted, for the server loop to hand to
    /// the blocking pool.
    pub fn take_blocking_job(&mut self) -> Option<BlockingJob> {
        self.pending_job.take()
    }

    /// Put a job back after the pool refused it. The connection stays paused
    /// and retries on the next tick.
    pub fn restore_blocking_job(&mut self, job: BlockingJob) {
        self.pending_job = Some(job);
    }

    /// Mark the taken job as queued/running. The next event is its completion.
    pub fn begin_blocking(&mut self) {
        self.blocking = true;
        self.paused.get_or_insert_with(Instant::now);
    }

    /// Deliver a blocking job's result and let the handler decide what is next.
    pub fn on_blocking_done(
        &mut self,
        outcome: BlockingOutcome,
        out: &mut Vec<u8>,
        out_at: &mut usize,
    ) -> bool {
        self.blocking = false;
        self.paused = None;
        self.touch();
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
        if !self.wants_read() {
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
            if self.blocking || self.pending_job.is_some() {
                // The handler is paused until its blocking job is submitted
                // and completes; do not deliver a Writable that could submit a
                // second job.
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
        if self.blocking || self.pending_job.is_some() {
            // A queued/running job may take longer than one poll tick, but it
            // must still make progress within the transfer timeout. This is
            // the bound that stops a queue-full retry from pinning the slot.
            if let Some(paused) = self.paused {
                if now.saturating_duration_since(paused) >= limits::current().transfer_timeout {
                    self.closing = Some(CloseReason::TimedOut);
                    let _ = flush(stream, out, out_at, last_active);
                    return false;
                }
            }
            return true;
        }
        if now < self.wakeup {
            return true;
        }
        if !self.on_event(StreamEvent::Tick(now), out, out_at) {
            return false;
        }
        if now < self.deadline {
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
            // A short protocol wakeup schedules a tick; it does not postpone
            // the server's no-progress bound, and a full output buffer still
            // closes the connection when that bound expires.
            self.wakeup = requested.min(self.deadline);
            let now = Instant::now();
            if self.wakeup < now {
                self.wakeup = now;
            }
        }
        match step.action {
            StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {}
            StreamAction::SubmitBlocking(job) => {
                self.pending_job = Some(job);
                self.paused.get_or_insert_with(Instant::now);
            }
            StreamAction::SendFile(send) => {
                // The server sends this range after the output buffer drains,
                // then delivers `Writable` again. A handler with more ranges
                // (a file bundle is many parts) can queue the next one; one
                // with nothing left returns `Close`.
                self.file = Some(send);
            }
            StreamAction::Close(reason) => {
                self.closing = Some(reason);
            }
        }
        if self.pending_job.is_none() && !self.blocking {
            self.paused = None;
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
                "file send ended before its advertised length",
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
    use std::fs::{self, File};
    use std::net::TcpListener;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn a_file_chunk_never_grows_past_the_stream_buffer() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "svcd-driver-chunk-{}-{}",
            std::process::id(),
            nanos
        ));
        fs::write(&path, [7u8; 64]).expect("write test file");
        let mut send = FileSend {
            file: File::open(&path).expect("open test file"),
            offset: 0,
            length: 64,
        };
        let mut out = Vec::new();
        assert_eq!(
            read_file_chunk(&mut send, &mut out, 16).expect("first chunk"),
            16
        );
        assert_eq!(out.len(), 16);
        assert_eq!(
            read_file_chunk(&mut send, &mut out, 16).expect("full buffer"),
            0
        );
        assert_eq!(
            out.len(),
            16,
            "a full output buffer stops the read at the high-water mark"
        );
        let _ = fs::remove_file(&path);
    }

    /// A handler that only waits for the next poll tick and never writes.
    /// This is the shape of an SSE handler between upstream progress events.
    struct IdleWaiter {
        ticks: usize,
        wait: Duration,
    }

    impl StreamHandler for IdleWaiter {
        fn on_event(&mut self, event: StreamEvent<'_>, _out: &mut OutBuffer<'_>) -> StreamStep {
            match event {
                StreamEvent::Tick(now) => {
                    self.ticks += 1;
                    StreamStep::wait(StreamAction::NeedRead).within(now + self.wait)
                }
                StreamEvent::Writable => {
                    _out.write(b"x");
                    StreamStep::wait(StreamAction::NeedWrite).within(Instant::now() + self.wait)
                }
                StreamEvent::Closed(_) => {
                    StreamStep::wait(StreamAction::Close(CloseReason::PeerClosed))
                }
                _ => StreamStep::wait(StreamAction::Continue),
            }
        }
    }

    /// A connected socket pair. The handler never reads or writes it in these
    /// tests; the driver only needs a real socket for its write path.
    fn socket() -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        TcpStream::connect(address).expect("connect")
    }

    fn driver(wait: Duration) -> StreamDriver {
        match StreamDriver::admit(Box::new(IdleWaiter { ticks: 0, wait }), usize::MAX) {
            StreamAdmission::Admitted(driver) => driver,
            StreamAdmission::Busy => panic!("an unlimited budget admits the handler"),
        }
    }

    #[test]
    fn a_short_wakeup_ticks_the_handler_without_shortening_the_bound() {
        let mut driver = driver(Duration::from_millis(100));
        let mut stream = socket();
        let mut out = Vec::new();
        let mut out_at = 0;
        let mut last_active = Instant::now();

        // The first writable event asks for a short poll wakeup.
        assert!(driver.on_writable(&mut stream, &mut out, &mut out_at, &mut last_active));
        let bound = driver.deadline;
        assert!(
            driver.wakeup < bound,
            "the handler asked for a shorter wakeup"
        );
        out.clear();
        out_at = 0;

        assert!(driver.on_tick(
            driver.wakeup + Duration::from_millis(1),
            &mut stream,
            &mut out,
            &mut out_at,
            &mut last_active,
        ));
        assert_eq!(
            driver.deadline, bound,
            "a poll tick must not replace the server's no-progress bound"
        );
    }

    #[test]
    fn an_idle_handler_still_closes_at_the_no_progress_bound() {
        let mut driver = driver(Duration::from_millis(100));
        let bound = driver.deadline;
        let mut stream = socket();
        let mut out = Vec::new();
        let mut out_at = 0;
        let mut last_active = Instant::now();

        assert!(
            !driver.on_tick(
                bound + Duration::from_millis(1),
                &mut stream,
                &mut out,
                &mut out_at,
                &mut last_active,
            ),
            "the handler cannot extend the bound by asking for a tick"
        );
        assert_eq!(driver.closing, Some(CloseReason::TimedOut));
    }

    #[test]
    fn output_progress_resets_the_bound_and_the_wakeup() {
        let mut driver = driver(Duration::from_millis(100));
        let bound = driver.deadline;
        let mut stream = socket();
        let mut out = Vec::new();
        let mut out_at = 0;
        let mut last_active = Instant::now();

        assert!(driver.on_writable(&mut stream, &mut out, &mut out_at, &mut last_active));
        assert!(!out.is_empty(), "the handler wrote a byte");
        assert!(
            driver.deadline > bound,
            "writing progress must push the no-progress bound forward"
        );
        assert!(
            driver.wakeup < driver.deadline,
            "the handler's short wakeup stays earlier than the bound"
        );
    }

    /// A handler that asks the pool for one job and resumes on its completion.
    struct SubmitOnce {
        submitted: bool,
    }

    impl StreamHandler for SubmitOnce {
        fn on_event(&mut self, event: StreamEvent<'_>, _out: &mut OutBuffer<'_>) -> StreamStep {
            match event {
                StreamEvent::Data(_) if !self.submitted => {
                    self.submitted = true;
                    StreamStep::wait(StreamAction::SubmitBlocking(Box::new(|| {
                        BlockingOutcome::Ok
                    })))
                }
                StreamEvent::BlockingDone(_) => StreamStep::wait(StreamAction::NeedRead),
                _ => StreamStep::wait(StreamAction::Continue),
            }
        }
    }

    #[test]
    fn a_blocking_job_pauses_input_until_its_completion() {
        let mut driver = match StreamDriver::admit(Box::new(SubmitOnce { submitted: false }), 1) {
            StreamAdmission::Admitted(driver) => driver,
            StreamAdmission::Busy => panic!("the budget admits the handler"),
        };
        let mut out = Vec::new();
        let mut out_at = 0;
        assert!(driver.on_data(b"body", &mut out, &mut out_at));
        assert!(
            !driver.wants_read(),
            "a submitted job pauses body reads until it finishes"
        );
        let job = driver
            .take_blocking_job()
            .expect("the handler submitted one job");
        driver.begin_blocking();
        assert!(!driver.wants_read(), "a running job keeps input paused");
        drop(job);
        assert!(driver.on_blocking_done(BlockingOutcome::Ok, &mut out, &mut out_at));
        assert!(
            driver.wants_read(),
            "the completed job lets the handler request the next body chunk"
        );
    }

    #[test]
    fn a_paused_job_still_closes_at_the_transfer_bound() {
        let mut driver = match StreamDriver::admit(Box::new(SubmitOnce { submitted: false }), 1) {
            StreamAdmission::Admitted(driver) => driver,
            StreamAdmission::Busy => panic!("the budget admits the handler"),
        };
        let mut out = Vec::new();
        let mut out_at = 0;
        assert!(driver.on_data(b"body", &mut out, &mut out_at));
        let _job = driver
            .take_blocking_job()
            .expect("the handler submitted one job");
        // The job was not submitted to a pool: it is a paused state, not a
        // running job. It still cannot pin the stream forever.
        let mut stream = socket();
        let mut last_active = Instant::now();
        let beyond = Instant::now() + limits::current().transfer_timeout + Duration::from_secs(1);
        assert!(!driver.on_tick(beyond, &mut stream, &mut out, &mut out_at, &mut last_active));
        assert_eq!(driver.closing, Some(CloseReason::TimedOut));
    }
}
