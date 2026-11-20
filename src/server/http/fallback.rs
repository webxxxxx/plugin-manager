//! Windows thread fallback: drive a sans-IO stream handler over a blocking
//! socket. The socket and its timeouts stay in the server layer; the face has
//! already produced the handler. A long-lived handler (SSE, WebSocket) is
//! driven full-duplex on this connection's thread: reads become `Data` events,
//! timeouts become `Tick` events, and a blocking job runs inline because this
//! thread is already dedicated to one connection.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::server::http::{
    CloseReason, FileSend, JobBudget, OutBuffer, StreamAction, StreamEvent, StreamHandler,
};

use super::budget::{Busy, StreamBudget};

/// How often an idle handler gets a tick so SSE can poll its bus and a ping
/// can be written. The Unix loop is woken by the bus/pool notify fd; the
/// fallback has no shared wake fd, so it polls at this cadence.
const IDLE_TICK: Duration = Duration::from_millis(250);

/// Drive one handler to completion on this connection thread. The face and
/// the request are the caller's job; this function owns the socket and the
/// stream-budget slot. `Err(Busy)` means nothing has been written yet.
pub fn drive(
    stream: &mut TcpStream,
    mut handler: Box<dyn StreamHandler>,
    initial: &[u8],
    budget: &StreamBudget,
    limits: &crate::platform::limits::Limits,
) -> Result<(), Busy> {
    let permit = budget.acquire(handler.stream_class(), limits.max_streams)?;
    let result = drive_loop(stream, handler.as_mut(), initial, limits);
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

fn drive_loop(
    stream: &mut TcpStream,
    handler: &mut dyn StreamHandler,
    initial: &[u8],
    limits: &crate::platform::limits::Limits,
) -> Result<(), Busy> {
    let limit = limits.stream_buffer;
    let timeout = limits.transfer_timeout;
    let mut out = Vec::new();
    let mut pending = initial;
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
                    step = handler
                        .on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
                    continue;
                }
                send_blocking(stream, send);
                // The handler may have another range (a file bundle) or may
                // close on the next `Writable`.
                step =
                    handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
            }
            // This thread is dedicated to one connection, so a "blocking" job
            // is just work that happens to take time. It never runs on a
            // shared server loop here.
            StreamAction::SubmitBlocking(job) => {
                // This thread is already the connection thread, so it cannot
                // observe a peer FIN while the job runs. The deadline still
                // bounds a stuck control round trip; the token stays clean.
                let outcome = job(JobBudget::for_fallback(timeout));
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
                if !pending.is_empty() {
                    let data = pending;
                    pending = &[];
                    step = handler.on_event(
                        StreamEvent::Data(data),
                        &mut OutBuffer::new(&mut out, limit),
                    );
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
    // Windows can hand the range to the socket directly. Every other host
    // copies; that path is unreachable while Unix uses its poll loop, but the
    // module compiles on both so the composition root needs no platform
    // branch (#139).
    #[cfg(windows)]
    let sent = crate::platform::host::zerocopy::send_file_range(&mut file, stream, offset, length)
        .unwrap_or(true);
    #[cfg(not(windows))]
    let sent = false;
    if !sent && file.seek(SeekFrom::Start(offset)).is_ok() {
        let mut reader = (&mut file).take(length);
        let _ = std::io::copy(&mut reader, stream);
    }
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::http::{BlockingOutcome, StreamStep};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

    struct BudgetHandler {
        captured: Option<mpsc::Sender<(Instant, bool)>>,
    }

    impl StreamHandler for BudgetHandler {
        fn on_event(&mut self, event: StreamEvent<'_>, _out: &mut OutBuffer<'_>) -> StreamStep {
            match event {
                StreamEvent::Writable => StreamStep::wait(StreamAction::NeedRead),
                StreamEvent::Data(_) => {
                    let captured = self.captured.take().expect("one data event");
                    StreamStep::wait(StreamAction::SubmitBlocking(Box::new(move |budget| {
                        let _ = captured.send((budget.deadline, budget.cancel.is_cancelled()));
                        BlockingOutcome::Ok(Vec::new())
                    })))
                }
                StreamEvent::BlockingDone(_) => {
                    StreamStep::wait(StreamAction::Close(CloseReason::Completed))
                }
                _ => StreamStep::wait(StreamAction::Continue),
            }
        }
    }

    /// The Windows fallback owns the connection thread, so it cannot observe a
    /// peer FIN while the job runs; the deadline still has to be set so a stuck
    /// control round trip cannot pin that thread forever.
    #[test]
    fn a_fallback_stream_job_gets_a_deadline_and_a_clean_token() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("addr");
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            std::io::Write::write_all(&mut stream, b"x").expect("write");
            std::thread::sleep(Duration::from_millis(100));
        });
        let (mut server, _) = listener.accept().expect("accept");
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        drive(
            &mut server,
            Box::new(BudgetHandler { captured: Some(tx) }),
            &[],
            &StreamBudget::new(),
            &crate::platform::limits::test_default(),
        )
        .expect("drive");
        let (deadline, cancelled) = rx.try_recv().expect("job budget");
        assert!(deadline > started, "the fallback must stamp a deadline");
        assert!(!cancelled, "the fallback has no disconnect signal");
        client.join().expect("client");
    }
}
