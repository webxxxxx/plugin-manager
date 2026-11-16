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
    CloseReason, FileSend, OutBuffer, StreamAction, StreamEvent, StreamHandler, StreamStep,
};

pub enum StreamAdmission {
    Admitted(StreamDriver),
    Busy,
}

/// One attached handler plus everything the connection needs to drive it.
pub struct StreamDriver {
    handler: Box<dyn StreamHandler>,
    /// The server's no-progress deadline, possibly shortened by the handler.
    deadline: Instant,
    /// A server-side file send requested by the handler, after the output
    /// buffer drains.
    file: Option<FileSend>,
    /// The handler asked to finish; flush the output buffer, then close.
    closing: Option<CloseReason>,
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
        StreamAdmission::Admitted(StreamDriver {
            handler,
            deadline: Instant::now() + limits::current().transfer_timeout,
            file: None,
            closing: None,
            _permit: permit,
        })
    }

    pub fn has_file(&self) -> bool {
        self.file.is_some()
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
        if now < self.deadline {
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
            if requested < self.deadline {
                self.deadline = requested;
            }
        }
        match step.action {
            StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {}
            // This slice has no blocking pool. The next server-loop slice
            // (#116) hands this to it instead of closing the response.
            StreamAction::SubmitBlocking(_) => {
                self.closing = Some(CloseReason::Failed(
                    "stream handlers cannot submit blocking jobs in this slice".into(),
                ));
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
