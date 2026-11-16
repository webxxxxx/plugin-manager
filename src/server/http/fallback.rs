//! Windows thread fallback: drive a sans-IO stream handler over a blocking
//! socket. The socket and its timeouts stay in the server layer; the face has
//! already produced the handler. The shared `max_streams` budget is acquired
//! before the first byte, exactly as the Unix driver does.

use std::io::Write;
use std::net::TcpStream;

use crate::limits;
use crate::server::http::{FileSend, OutBuffer, StreamAction, StreamEvent, StreamHandler};

use super::budget::{Busy, StreamBudget};

/// Drive one handler to completion on this connection thread. The face and
/// the request are the caller's job; this function only owns the socket and
/// the stream-budget slot. `Err(Busy)` means nothing has been written yet.
pub fn drive(stream: &mut TcpStream, mut handler: Box<dyn StreamHandler>) -> Result<(), Busy> {
    let _permit =
        StreamBudget::global().acquire(handler.stream_class(), limits::current().max_streams)?;
    let limit = limits::current().stream_buffer;
    let mut out = Vec::new();
    loop {
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
        let wrote = !out.is_empty();
        if wrote && stream.write_all(&out).is_err() {
            return Ok(());
        }
        out.clear();
        match step.action {
            StreamAction::Close(_) => return Ok(()),
            StreamAction::SendFile(send) => {
                if send.length == 0 {
                    // Same guard as the Unix driver: an empty range makes no
                    // progress, so ask the handler for its next action instead
                    // of spinning this connection thread.
                    continue;
                }
                send_blocking(stream, send);
                // The handler may have another range (a file bundle) or may
                // close on the next `Writable`.
                continue;
            }
            // This slice has no blocking pool. The Unix server loop would hand
            // the job to it (#116); the fallback must not run it on this thread.
            StreamAction::SubmitBlocking(_) => {
                let _ = stream.write_all(&crate::http::json_error(
                    500,
                    "internal",
                    "blocking jobs are not available on the thread fallback",
                ));
                return Ok(());
            }
            StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {
                if !wrote {
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
