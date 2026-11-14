//! NDJSON framing for the plugin control channel.
//!
//! The same JSON-RPC 2.0 envelope runs over every transport. A frame is one
//! UTF-8 JSON line terminated by LF, with no embedded newline. The writer never
//! emits CR, but a plugin launched through cmd.exe may still produce CRLF;
//! serde_json treats a trailing CR as whitespace, so the reader keeps it to
//! avoid silently rewriting bytes.

use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread;

use crate::threads;
use crate::wake::Wake;

/// One control-plane JSON-RPC message (ping, status, cancel) may not exceed
/// this many bytes. Keeping it small makes accidental bulk data visible at the
/// protocol boundary instead of letting one message pin a large buffer.
pub const CONTROL_FRAME_MAX: usize = 256 * 1024;
/// One inline tool-call request/response may not exceed this many bytes.
/// Larger tool payloads belong in the blob data plane, not in a control frame.
pub const CALL_FRAME_MAX: usize = 1024 * 1024;
/// One catalog page. Pages are intentionally smaller than a call frame so a
/// page is never the reason the MCP bridge hits its output limit.
pub const CATALOG_PAGE_MAX: usize = 256 * 1024;
/// The legacy/local management channel carries small JSON-RPC envelopes.
pub const LOCAL_FRAME_MAX: usize = 1024 * 1024;
/// Backwards-compatible default for call sites that have not chosen a class.
pub const MAX_FRAME_BYTES: usize = CALL_FRAME_MAX;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// The peer produced a frame larger than the limit for this transport.
    TooLarge {
        max: usize,
    },
    /// The peer produced bytes that are not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(error) => write!(formatter, "{}", error),
            FrameError::TooLarge { max } => write!(formatter, "frame exceeds {} bytes", max),
            FrameError::InvalidUtf8 => write!(formatter, "frame is not valid UTF-8"),
        }
    }
}

impl From<FrameError> for io::Error {
    fn from(error: FrameError) -> Self {
        match error {
            FrameError::Io(error) => error,
            FrameError::TooLarge { max } => io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame exceeds {} bytes", max),
            ),
            FrameError::InvalidUtf8 => {
                io::Error::new(io::ErrorKind::InvalidData, "frame is not valid UTF-8")
            }
        }
    }
}

/// Read exactly one LF-terminated frame. The returned string excludes the LF
/// but keeps every other byte, including a trailing CR.
#[allow(dead_code)]
pub fn read_frame<R: BufRead + ?Sized>(reader: &mut R) -> Result<Option<String>, FrameError> {
    read_frame_limit(reader, MAX_FRAME_BYTES)
}

/// Read one frame with an explicit transport limit. The buffer never grows
/// beyond `max` bytes; an oversized peer is reported without consuming the
/// rest of the stream.
pub fn read_frame_limit<R: BufRead + ?Sized>(
    reader: &mut R,
    max: usize,
) -> Result<Option<String>, FrameError> {
    let mut bytes = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(FrameError::Io(error)),
        };
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(None);
            }
            return Err(FrameError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unterminated frame",
            )));
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            if bytes.len() + position > max {
                return Err(FrameError::TooLarge { max });
            }
            bytes.extend_from_slice(&available[..position]);
            reader.consume(position + 1);
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| FrameError::InvalidUtf8);
        }
        if bytes.len() + available.len() > max {
            return Err(FrameError::TooLarge { max });
        }
        let length = available.len();
        bytes.extend_from_slice(available);
        reader.consume(length);
    }
}

/// Read one protocol-v2 frame: a 4-byte big-endian length followed by exactly
/// that many UTF-8 bytes. A clean EOF before the length prefix is `Ok(None)`.
pub fn read_length_frame_limit<R: Read + ?Sized>(
    reader: &mut R,
    max: usize,
) -> Result<Option<String>, FrameError> {
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(FrameError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated length prefix",
                )))
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > max {
        return Err(FrameError::TooLarge { max });
    }
    let mut bytes = vec![0u8; length];
    let mut filled = 0;
    while filled < bytes.len() {
        match reader.read(&mut bytes[filled..]) {
            Ok(0) => {
                return Err(FrameError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated frame payload",
                )))
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| FrameError::InvalidUtf8)
}

/// Write one protocol-v2 frame.
pub fn write_length_frame_limit<W: Write + ?Sized>(
    writer: &mut W,
    frame: &str,
    max: usize,
) -> io::Result<()> {
    if frame.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame exceeds the {} byte limit", max),
        ));
    }
    let length = u32::try_from(frame.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame is too large"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(frame.as_bytes())?;
    writer.flush()
}

/// Write one LF-terminated frame. Callers pass serialized JSON, so an embedded
/// newline would mean the contract was broken before this point.
pub fn write_frame<W: Write + ?Sized>(writer: &mut W, frame: &str) -> io::Result<()> {
    write_frame_limit(writer, frame, MAX_FRAME_BYTES)
}

/// Write one frame with an explicit transport limit.
pub fn write_frame_limit<W: Write + ?Sized>(
    writer: &mut W,
    frame: &str,
    max: usize,
) -> io::Result<()> {
    if frame.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame exceeds the {} byte limit", max),
        ));
    }
    if frame.contains('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame contains an embedded newline",
        ));
    }
    writer.write_all(frame.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// Events produced by a plugin's stdout reader thread.
#[derive(Debug)]
pub enum FrameEvent {
    Frame(Result<String, FrameError>),
    Closed,
}

/// Read frames from the stream on a dedicated thread and forward them to the
/// daemon. The reader stops at EOF, a protocol violation, or when the receiver
/// is dropped.
pub fn spawn_reader<R: Read + Send + 'static>(
    name: String,
    stream: R,
    sender: SyncSender<FrameEvent>,
    wake: Option<Arc<Wake>>,
) -> io::Result<()> {
    spawn_reader_limit(name, stream, sender, wake, MAX_FRAME_BYTES)
}

/// Spawn a frame reader with an explicit transport limit. Plugins use the
/// inline call limit; the MCP bridge uses the same limit for its stdout.
/// The sender is bounded, so a slow consumer applies backpressure to the
/// reader thread instead of growing an unbounded queue.
pub fn spawn_reader_limit<R: Read + Send + 'static>(
    name: String,
    stream: R,
    sender: SyncSender<FrameEvent>,
    wake: Option<Arc<Wake>>,
    max: usize,
) -> io::Result<()> {
    thread::Builder::new()
        .name(name)
        .stack_size(threads::IO_STACK)
        .spawn(move || {
            let mut reader = io::BufReader::new(stream);
            loop {
                let event = match read_frame_limit(&mut reader, max) {
                    Ok(Some(line)) => FrameEvent::Frame(Ok(line)),
                    Ok(None) => FrameEvent::Closed,
                    Err(error) => FrameEvent::Frame(Err(error)),
                };
                let done = matches!(event, FrameEvent::Closed | FrameEvent::Frame(Err(_)));
                if sender.send(event).is_err() {
                    break;
                }
                if let Some(wake) = &wake {
                    wake.notify();
                }
                if done {
                    break;
                }
            }
        })
        .map(|_| ())
}

/// Protocol-v2 reader: 4-byte length prefix instead of LF framing.
pub fn spawn_length_reader_limit<R: Read + Send + 'static>(
    name: String,
    stream: R,
    sender: SyncSender<FrameEvent>,
    wake: Option<Arc<Wake>>,
    max: usize,
) -> io::Result<()> {
    thread::Builder::new()
        .name(name)
        .stack_size(threads::IO_STACK)
        .spawn(move || {
            let mut reader = io::BufReader::new(stream);
            loop {
                let event = match read_length_frame_limit(&mut reader, max) {
                    Ok(Some(line)) => FrameEvent::Frame(Ok(line)),
                    Ok(None) => FrameEvent::Closed,
                    Err(error) => FrameEvent::Frame(Err(error)),
                };
                let done = matches!(event, FrameEvent::Closed | FrameEvent::Frame(Err(_)));
                if sender.send(event).is_err() {
                    break;
                }
                if let Some(wake) = &wake {
                    wake.notify();
                }
                if done {
                    break;
                }
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn read_all(input: &[u8]) -> Vec<Result<Option<String>, FrameError>> {
        let mut reader = BufReader::new(input);
        let mut frames = Vec::new();
        loop {
            let frame = read_frame(&mut reader);
            let done = matches!(frame, Ok(None) | Err(_));
            frames.push(frame);
            if done {
                break;
            }
        }
        frames
    }

    #[test]
    fn reads_lf_delimited_frames() {
        let frames = read_all(b"{\"jsonrpc\":\"2.0\"}\n{\"id\":1}\n");

        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0].as_ref().expect("frame").as_deref(),
            Some("{\"jsonrpc\":\"2.0\"}")
        );
        assert_eq!(
            frames[1].as_ref().expect("frame").as_deref(),
            Some("{\"id\":1}")
        );
        assert!(matches!(frames[2], Ok(None)));
    }

    #[test]
    fn accepts_a_trailing_cr_from_windows_scripts() {
        let frames = read_all(b"{\"id\":1}\r\n");

        assert_eq!(
            frames[0].as_ref().expect("frame").as_deref(),
            Some("{\"id\":1}\r")
        );
    }

    #[test]
    fn a_frame_at_the_limit_is_accepted() {
        let payload = vec![b'x'; MAX_FRAME_BYTES];
        let mut input = payload.clone();
        input.push(b'\n');

        let frame = read_frame(&mut BufReader::new(input.as_slice()))
            .expect("read")
            .expect("frame");

        assert_eq!(frame.len(), MAX_FRAME_BYTES);
    }

    #[test]
    fn a_frame_over_the_limit_is_a_protocol_violation() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES + 1];
        input.push(b'\n');

        assert!(matches!(
            read_frame(&mut BufReader::new(input.as_slice())),
            Err(FrameError::TooLarge {
                max: MAX_FRAME_BYTES
            })
        ));
    }

    #[test]
    fn an_explicit_limit_is_enforced_for_reads_and_writes() {
        let mut input = b"0123456789\n".to_vec();
        assert!(matches!(
            read_frame_limit(&mut BufReader::new(input.as_slice()), 10),
            Ok(Some(frame)) if frame == "0123456789"
        ));
        input.truncate(10);
        assert!(matches!(
            read_frame_limit(&mut BufReader::new(input.as_slice()), 9),
            Err(FrameError::TooLarge { max: 9 })
        ));
        assert!(write_frame_limit(&mut Vec::new(), "0123456789", 10).is_ok());
        assert!(write_frame_limit(&mut Vec::new(), "0123456789", 9).is_err());
    }

    #[test]
    fn invalid_utf8_is_reported() {
        assert!(matches!(
            read_frame(&mut BufReader::new([0xffu8, b'\n'].as_slice())),
            Err(FrameError::InvalidUtf8)
        ));
    }

    #[test]
    fn a_partial_frame_at_eof_is_not_treated_as_a_frame() {
        assert!(matches!(
            read_frame(&mut BufReader::new(b"{\"id\":1}".as_slice())),
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn length_frames_round_trip_and_enforce_the_limit() {
        let mut output = Vec::new();
        write_length_frame_limit(&mut output, "{\"id\":1}", 64).expect("write");
        assert_eq!(&output[..4], &(8u32).to_be_bytes());
        let mut input = output.as_slice();
        assert_eq!(
            read_length_frame_limit(&mut input, 64).expect("read"),
            Some("{\"id\":1}".to_string())
        );

        let mut oversized = Vec::new();
        write_length_frame_limit(&mut oversized, "0123456789", 64).expect("write");
        assert!(matches!(
            read_length_frame_limit(&mut oversized.as_slice(), 9),
            Err(FrameError::TooLarge { max: 9 })
        ));
    }

    #[test]
    fn writes_one_lf_terminated_frame() {
        let mut output = Vec::new();
        write_frame(&mut output, "{\"id\":1}").expect("write");
        assert_eq!(output, b"{\"id\":1}\n");
    }

    #[test]
    fn write_rejects_embedded_newlines_and_oversize_frames() {
        assert!(write_frame(&mut Vec::new(), "one\ntwo").is_err());
        let oversized = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(write_frame(&mut Vec::new(), &oversized).is_err());
    }
}
