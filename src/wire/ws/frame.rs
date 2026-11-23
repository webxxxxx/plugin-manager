//! WebSocket framing shared by the plugin UI stream handlers.
//!
//! Framing is pure byte work: parse a client frame out of a buffer, or encode
//! a server frame into one. The stream handlers own the buffers; this module
//! owns the RFC 6455 shape (masking, lengths, control-frame rules).

/// One complete client frame.
#[derive(Debug)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// Why a client frame could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The frame violates RFC 6455 (reserved bits, unmasked, bad control frame).
    Protocol(&'static str),
    /// The announced payload is over the handler's limit. `total` is the full
    /// wire size so the caller can drain the rest before closing.
    TooLarge { total: usize },
}

/// Parse one frame from `buffer`. `Ok(None)` means more bytes are needed;
/// `Ok(Some((frame, used)))` consumes exactly `used` bytes.
pub fn parse_frame(buffer: &[u8], max: usize) -> Result<Option<(Frame, usize)>, ParseError> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    let fin = buffer[0] & 0x80 != 0;
    let opcode = buffer[0] & 0x0F;
    if buffer[0] & 0x70 != 0 {
        return Err(ParseError::Protocol("reserved frame bits"));
    }
    if buffer[1] & 0x80 == 0 {
        return Err(ParseError::Protocol("client frames must be masked"));
    }
    let mut length = u64::from(buffer[1] & 0x7F);
    let mut at = 2;
    if length == 126 {
        if buffer.len() < at + 2 {
            return Ok(None);
        }
        length = u64::from(u16::from_be_bytes([buffer[at], buffer[at + 1]]));
        at += 2;
    } else if length == 127 {
        if buffer.len() < at + 8 {
            return Ok(None);
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&buffer[at..at + 8]);
        length = u64::from_be_bytes(bytes);
        at += 8;
    }
    if length > max as u64 {
        return Err(ParseError::TooLarge {
            total: at + 4 + length as usize,
        });
    }
    if opcode >= 0x8 && (!fin || length > 125) {
        return Err(ParseError::Protocol("invalid control frame"));
    }
    if buffer.len() < at + 4 {
        return Ok(None);
    }
    let mask = [buffer[at], buffer[at + 1], buffer[at + 2], buffer[at + 3]];
    at += 4;
    let total = at + length as usize;
    if buffer.len() < total {
        return Ok(None);
    }
    let mut payload = buffer[at..total].to_vec();
    for (index, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[index % 4];
    }
    Ok(Some((
        Frame {
            fin,
            opcode,
            payload,
        },
        total,
    )))
}

/// The payload of a close frame: the 16-bit status code followed by an
/// optional UTF-8 reason.
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut payload = code.to_be_bytes().to_vec();
    payload.extend_from_slice(reason.as_bytes());
    payload
}

/// Why a peer's close payload is not a valid close frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseError {
    /// A one-byte payload or a reserved/unassigned status code.
    Protocol(&'static str),
    /// The optional reason is not valid UTF-8.
    InvalidUtf8,
}

/// Extract the status code from a close payload.
///
/// An empty payload means no code (1005 locally) and is accepted. A one-byte
/// payload is malformed. Reserved codes 1004/1005/1006/1015, codes below 1000,
/// and the unassigned 1016..=2999 range are rejected; 3000..=4999 are
/// application codes and accepted.
pub fn close_code(payload: &[u8]) -> Result<Option<u16>, CloseError> {
    match payload.len() {
        0 => Ok(None),
        1 => Err(CloseError::Protocol("close frame payload length 1")),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            if !matches!(code, 1000..=1003 | 1007..=1014 | 3000..=4999) {
                return Err(CloseError::Protocol("invalid close code"));
            }
            if std::str::from_utf8(&payload[2..]).is_err() {
                return Err(CloseError::InvalidUtf8);
            }
            Ok(Some(code))
        }
    }
}

/// Encode the frame header for `payload`.
pub fn frame_header(opcode: u8, length: usize) -> Vec<u8> {
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | opcode);
    if length < 126 {
        header.push(length as u8);
    } else if length <= u16::MAX as usize {
        header.push(126);
        header.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(length as u64).to_be_bytes());
    }
    header
}

/// Split a written stream into the HTTP head and its server frames.
///
/// Shared by the raw and WS stream handler tests; both write a 101 head and
/// then unmasked server frames into the same buffer.
#[cfg(test)]
pub(crate) fn server_frames(bytes: &[u8]) -> (String, Vec<(u8, Vec<u8>)>) {
    let head_end = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    let head = head_end
        .map(|end| String::from_utf8_lossy(&bytes[..end + 4]).to_string())
        .unwrap_or_default();
    let mut body = head_end.map(|end| &bytes[end + 4..]).unwrap_or(bytes);
    let mut frames = Vec::new();
    while body.len() >= 2 {
        let opcode = body[0] & 0x0F;
        let mut length = u64::from(body[1] & 0x7F);
        let mut at = 2;
        if length == 126 {
            length = u64::from(u16::from_be_bytes([body[at], body[at + 1]]));
            at += 2;
        } else if length == 127 {
            let mut extended = [0u8; 8];
            extended.copy_from_slice(&body[at..at + 8]);
            length = u64::from_be_bytes(extended);
            at += 8;
        }
        let payload = body[at..at + length as usize].to_vec();
        frames.push((opcode, payload));
        body = &body[at + length as usize..];
    }
    (head, frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_frame(opcode: u8, payload: &[u8], fin: bool) -> Vec<u8> {
        let mask = [1u8, 2, 3, 4];
        let mut bytes = frame_header(opcode, payload.len());
        if !fin {
            bytes[0] &= 0x7F;
        }
        bytes[1] |= 0x80;
        bytes.extend_from_slice(&mask);
        bytes.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        bytes
    }

    #[test]
    fn a_masked_client_frame_round_trips() {
        let bytes = client_frame(0x1, b"hello", true);
        let (frame, used) = parse_frame(&bytes, 1024).expect("frame").expect("complete");
        assert_eq!(used, bytes.len());
        assert!(frame.fin);
        assert_eq!(frame.opcode, 0x1);
        assert_eq!(frame.payload, b"hello");
    }

    #[test]
    fn a_split_frame_waits_for_more_bytes() {
        let bytes = client_frame(0x2, b"chunk", true);
        for split in 1..bytes.len() {
            assert!(
                parse_frame(&bytes[..split], 1024)
                    .expect("prefix")
                    .is_none(),
                "split at {split} must wait"
            );
        }
        assert!(parse_frame(&bytes, 1024).expect("whole").is_some());
    }

    #[test]
    fn unmasked_and_reserved_bit_frames_are_protocol_errors() {
        let mut bytes = client_frame(0x1, b"x", true);
        bytes[1] &= 0x7F;
        assert_eq!(
            parse_frame(&bytes, 1024).err(),
            Some(ParseError::Protocol("client frames must be masked"))
        );
        let mut bytes = client_frame(0x1, b"x", true);
        bytes[0] |= 0x10;
        assert_eq!(
            parse_frame(&bytes, 1024).err(),
            Some(ParseError::Protocol("reserved frame bits"))
        );
    }

    #[test]
    fn an_oversized_frame_reports_its_total_wire_size() {
        let payload = vec![0x41; 64];
        let bytes = client_frame(0x2, &payload, true);
        assert_eq!(
            parse_frame(&bytes, 32).err(),
            Some(ParseError::TooLarge { total: bytes.len() })
        );
    }

    #[test]
    fn close_payloads_carry_the_status_code_and_reason() {
        assert_eq!(
            close_payload(1009, "too large"),
            vec![0x03, 0xF1, b't', b'o', b'o', b' ', b'l', b'a', b'r', b'g', b'e']
        );
    }

    #[test]
    fn close_codes_are_validated() {
        assert_eq!(close_code(&[]), Ok(None));
        assert_eq!(close_code(&1000u16.to_be_bytes()), Ok(Some(1000)));
        assert_eq!(close_code(&[0x03, 0xE8, b'b', b'y', b'e']), Ok(Some(1000)));
        for code in [999u16, 1004, 1005, 1006, 1015, 1016, 2999] {
            assert_eq!(
                close_code(&code.to_be_bytes()),
                Err(CloseError::Protocol("invalid close code")),
                "{code}"
            );
        }
        for code in [1000u16, 1001, 1002, 1003, 1007, 1014, 3000, 4999] {
            assert_eq!(close_code(&code.to_be_bytes()), Ok(Some(code)), "{code}");
        }
        assert_eq!(
            close_code(&[0x03]),
            Err(CloseError::Protocol("close frame payload length 1"))
        );
        assert_eq!(
            close_code(&[0x03, 0xE8, 0xFF]),
            Err(CloseError::InvalidUtf8)
        );
    }

    #[test]
    fn frame_headers_use_the_shortest_encoding() {
        assert_eq!(frame_header(0x1, 5), vec![0x81, 5]);
        assert_eq!(frame_header(0x1, 300), {
            let mut expected = vec![0x81, 126];
            expected.extend_from_slice(&300u16.to_be_bytes());
            expected
        });
        let header = frame_header(0x2, 70_000);
        assert_eq!(&header[..2], &[0x82, 127]);
        assert_eq!(header.len(), 10);
    }
}
