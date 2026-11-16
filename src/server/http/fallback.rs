//! Windows thread fallback: drive a sans-IO stream handler over a blocking
//! socket. The socket and its timeouts stay in the server layer; the face has
//! already produced the handler. The shared `max_streams` budget is acquired
//! before the first byte, exactly as the Unix driver does.
//!
//! Upload handlers need request-body bytes and blocking staging jobs. This
//! driver reads on `NeedRead` and runs `SubmitBlocking` inline: the connection
//! thread is already dedicated to one request, so a slow disk only blocks this
//! connection.

use std::io::{Read, Write};
use std::net::TcpStream;

use crate::limits;
use crate::server::http::{FileSend, OutBuffer, StreamAction, StreamEvent, StreamHandler};

use super::budget::{Busy, StreamBudget};

/// Drive one handler to completion on this connection thread. The face and
/// the request are the caller's job; this function only owns the socket and
/// the stream-budget slot. `Err(Busy)` means nothing has been written yet.
pub fn drive(
    stream: &mut TcpStream,
    mut handler: Box<dyn StreamHandler>,
    initial: &[u8],
) -> Result<(), Busy> {
    let _permit =
        StreamBudget::global().acquire(handler.stream_class(), limits::current().max_streams)?;
    let limit = limits::current().stream_buffer;
    let mut out = Vec::new();
    let mut pending = initial;
    let mut chunk = vec![0u8; 8 * 1024];
    let mut event = StreamEvent::Writable;
    loop {
        let step = handler.on_event(event, &mut OutBuffer::new(&mut out, limit));
        let wrote = !out.is_empty();
        if wrote && stream.write_all(&out).is_err() {
            return Ok(());
        }
        out.clear();
        match step.action {
            StreamAction::Close(_) => return Ok(()),
            StreamAction::SendFile(send) => {
                if send.length > 0 {
                    send_blocking(stream, send);
                }
                event = StreamEvent::Writable;
            }
            StreamAction::SubmitBlocking(job) => {
                event = StreamEvent::BlockingDone(job());
            }
            StreamAction::NeedRead => {
                if !pending.is_empty() {
                    let data = pending;
                    pending = &[];
                    event = StreamEvent::Data(data);
                    continue;
                }
                match stream.read(&mut chunk) {
                    Ok(0) => return Ok(()),
                    Ok(read) => {
                        event = StreamEvent::Data(&chunk[..read]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Ok(())
                    }
                    Err(_) => return Ok(()),
                }
            }
            StreamAction::NeedWrite => {
                // The handler queued output (a response head) without writing
                // it in this step; give it the next Writable.
                event = StreamEvent::Writable;
            }
            StreamAction::Continue => {
                if !pending.is_empty() {
                    let data = pending;
                    pending = &[];
                    event = StreamEvent::Data(data);
                } else if wrote {
                    event = StreamEvent::Writable;
                } else {
                    return Ok(());
                }
            }
        }
    }
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
