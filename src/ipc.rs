//! NDJSON framing for the plugin control channel.
//!
//! The same JSON-RPC 2.0 envelope runs over every transport. A frame is one
//! UTF-8 JSON line terminated by LF, with no embedded newline. The writer never
//! emits CR, but a plugin launched through cmd.exe may still produce CRLF;
//! serde_json treats a trailing CR as whitespace, so the reader keeps it to
//! avoid silently rewriting bytes.

use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use crate::wake::Wake;

/// One JSON-RPC message may not exceed this many bytes, excluding the LF.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// The peer produced a frame larger than MAX_FRAME_BYTES.
    TooLarge,
    /// The peer produced bytes that are not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(error) => write!(formatter, "{}", error),
            FrameError::TooLarge => write!(formatter, "frame exceeds {} bytes", MAX_FRAME_BYTES),
            FrameError::InvalidUtf8 => write!(formatter, "frame is not valid UTF-8"),
        }
    }
}

/// Read exactly one LF-terminated frame. The returned string excludes the LF
/// but keeps every other byte, including a trailing CR.
pub fn read_frame<R: BufRead + ?Sized>(reader: &mut R) -> Result<Option<String>, FrameError> {
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
            if bytes.len() + position > MAX_FRAME_BYTES {
                return Err(FrameError::TooLarge);
            }
            bytes.extend_from_slice(&available[..position]);
            reader.consume(position + 1);
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|_| FrameError::InvalidUtf8);
        }
        if bytes.len() + available.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        let length = available.len();
        bytes.extend_from_slice(available);
        reader.consume(length);
    }
}

/// Write one LF-terminated frame. Callers pass serialized JSON, so an embedded
/// newline would mean the contract was broken before this point.
pub fn write_frame<W: Write + ?Sized>(writer: &mut W, frame: &str) -> io::Result<()> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds the 1 MiB limit",
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
    sender: Sender<FrameEvent>,
    wake: Option<Arc<Wake>>,
) -> io::Result<()> {
    thread::Builder::new()
        .name(name)
        .spawn(move || {
            let mut reader = io::BufReader::new(stream);
            loop {
                let event = match read_frame(&mut reader) {
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
            Err(FrameError::TooLarge)
        ));
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
