//! Windows thread fallback: drive a sans-IO stream handler over a blocking
//! socket. The socket and its timeouts stay in the server layer; the face has
//! already produced the handler. A long-lived handler (SSE, WebSocket) is
//! driven full-duplex on this connection's thread: reads become `Data` events,
//! timeouts become `Tick` events, and a blocking job runs inline because this
//! thread is already dedicated to one connection.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::limits;
use crate::server::http::{
    CloseReason, FileSend, OutBuffer, StreamAction, StreamEvent, StreamHandler,
};

use super::budget::{Busy, StreamBudget};

/// How often an idle handler gets a tick so SSE can poll its bus and a ping
/// can be written. The Unix loop is woken by the bus/pool notify fd; the
/// fallback has no shared wake fd, so it polls at this cadence.
const IDLE_TICK: Duration = Duration::from_millis(250);

/// Drive one handler to completion on this connection thread. The face and
/// the request are the caller's job; this function owns the socket and the
/// stream-budget slot. `Err(Busy)` means nothing has been written yet.
pub fn drive(stream: &mut TcpStream, mut handler: Box<dyn StreamHandler>) -> Result<(), Busy> {
    let permit =
        StreamBudget::global().acquire(handler.stream_class(), limits::current().max_streams)?;
    let result = drive_loop(stream, handler.as_mut());
    // The Unix driver delivers this from its Drop; the fallback owns the
    // handler directly, so it must deliver the same final event here.
    let mut discarded = Vec::new();
    let _ = handler.on_event(
        StreamEvent::Closed(CloseReason::PeerClosed),
        &mut OutBuffer::new(&mut discarded, 0),
    );
    drop(permit);
    result
}

fn drive_loop(stream: &mut TcpStream, handler: &mut dyn StreamHandler) -> Result<(), Busy> {
    let limit = limits::current().stream_buffer;
    let timeout = limits::current().transfer_timeout;
    let mut out = Vec::new();
    let mut read_buf = [0u8; 8 * 1024];
    let mut last_active = Instant::now();
    let mut deadline = last_active + timeout;
    let mut step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
    loop {
        let wrote = match flush(stream, &mut out) {
            Ok(wrote) => wrote,
            Err(_) => return Ok(()),
        };
        if wrote {
            last_active = Instant::now();
            deadline = last_active + timeout;
        }
        match step.action {
            StreamAction::Close(_) => {
                let _ = flush(stream, &mut out);
                return Ok(());
            }
            StreamAction::SendFile(send) => {
                if send.length == 0 {
                    // Same guard as the Unix driver: an empty range makes no
                    // progress, so ask for the next action instead of spinning.
                    step = handler.on_event(
                        StreamEvent::Writable,
                        &mut OutBuffer::new(&mut out, limit),
                    );
                    continue;
                }
                send_blocking(stream, send);
                // The handler may have another range (a file bundle) or may
                // close on the next `Writable`.
                step = handler.on_event(
                    StreamEvent::Writable,
                    &mut OutBuffer::new(&mut out, limit),
                );
            }
            // This thread is dedicated to one connection, so a "blocking" job
            // is just work that happens to take time. It never runs on a
            // shared server loop here.
            StreamAction::SubmitBlocking(job) => {
                let outcome = job();
                step = handler.on_event(
                    StreamEvent::BlockingDone(outcome),
                    &mut OutBuffer::new(&mut out, limit),
                );
            }
            StreamAction::Continue | StreamAction::NeedWrite => {
                if wrote {
                    // The handler may have more buffered output; give it the
                    // writable event the Unix loop would deliver next.
                    step = handler
                        .on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
                    continue;
                }
                let now = Instant::now();
                let next = step.deadline.unwrap_or(deadline).min(deadline);
                if now >= next {
                    step = handler
                        .on_event(StreamEvent::Tick(now), &mut OutBuffer::new(&mut out, limit));
                    if now >= step.deadline.unwrap_or(deadline) && out.is_empty() {
                        return Ok(());
                    }
                    continue;
                }
                let wait = (next - now).min(IDLE_TICK);
                std::thread::sleep(wait);
                let now = Instant::now();
                step =
                    handler.on_event(StreamEvent::Tick(now), &mut OutBuffer::new(&mut out, limit));
            }
            StreamAction::NeedRead => {
                let now = Instant::now();
                let next = step.deadline.unwrap_or(deadline).min(deadline);
                if now >= next {
                    step = handler
                        .on_event(StreamEvent::Tick(now), &mut OutBuffer::new(&mut out, limit));
                    if now >= step.deadline.unwrap_or(deadline) && out.is_empty() {
                        return Ok(());
                    }
                    continue;
                }
                let wait = (next - now).min(IDLE_TICK);
                if stream
                    .set_read_timeout(Some(wait.max(Duration::from_millis(1))))
                    .is_err()
                {
                    return Ok(());
                }
                match stream.read(&mut read_buf) {
                    Ok(0) => return Ok(()),
                    Ok(read) => {
                        last_active = Instant::now();
                        deadline = last_active + timeout;
                        step = handler.on_event(
                            StreamEvent::Data(&read_buf[..read]),
                            &mut OutBuffer::new(&mut out, limit),
                        );
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            || error.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        step = handler.on_event(
                            StreamEvent::Tick(Instant::now()),
                            &mut OutBuffer::new(&mut out, limit),
                        );
                    }
                    Err(_) => return Ok(()),
                }
            }
        }
    }
}

/// Write the handler's output without an unbounded intermediate buffer.
fn flush(stream: &mut TcpStream, out: &mut Vec<u8>) -> std::io::Result<bool> {
    if out.is_empty() {
        return Ok(false);
    }
    stream.write_all(out)?;
    out.clear();
    Ok(true)
}

fn send_blocking(stream: &mut TcpStream, send: FileSend) {
    use std::io::{Read, Seek, SeekFrom, Write};

    let FileSend {
        mut file,
        offset,
        length,
    } = send;
    if length == 0 {
        return;
    }
    match crate::zerocopy::send_file_range(&mut file, stream, offset, length) {
        Ok(true) => {}
        Ok(false) => {
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let mut reader = (&mut file).take(length);
                let _ = std::io::copy(&mut reader, stream);
            }
        }
        Err(_) => {}
    }
    let _ = stream.flush();
}
