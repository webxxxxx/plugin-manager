//! RFC 6455 opening handshake: validate the client's headers and compute
//! `Sec-WebSocket-Accept`.
//!
//! Moved out of `face/uichannel.rs` (#206): the handshake is byte format, not
//! UI channel logic, and the plugin UI streams need it on every platform.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Validate the RFC 6455 opening handshake and return the value for
/// `Sec-WebSocket-Accept`.
pub fn ws_accept(headers: &[(String, String)]) -> Result<String, &'static str> {
    let upgrade = header(headers, "upgrade").unwrap_or("");
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err("Upgrade must be websocket");
    }
    let connection = header(headers, "connection").unwrap_or("");
    if !connection
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
    {
        return Err("Connection must include Upgrade");
    }
    if header(headers, "sec-websocket-version") != Some("13") {
        return Err("Sec-WebSocket-Version must be 13");
    }
    let key = header(headers, "sec-websocket-key").ok_or("missing Sec-WebSocket-Key")?;
    let decoded = BASE64
        .decode(key.trim())
        .map_err(|_| "invalid Sec-WebSocket-Key")?;
    if decoded.len() != 16 {
        return Err("invalid Sec-WebSocket-Key");
    }
    Ok(accept_key(key.trim()))
}

pub fn accept_key(key: &str) -> String {
    BASE64.encode(sha1(format!("{}{}", key, WS_GUID).as_bytes()))
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Minimal SHA-1 (RFC 3174) for the WebSocket accept key. The rest of the
/// project only needs SHA-256; adding a crate for one 20-byte digest would cost
/// binary size for no protocol benefit.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut message = data.to_vec();
    let bit_length = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    let mut h = [
        0x6745_2301u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    for chunk in message.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (index, word) in words.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        for (index, word) in words.iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut digest = [0u8; 20];
    for (index, word) in h.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::new();
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(out, "{:02x}", byte);
        }
        out
    }

    #[test]
    fn sha1_matches_the_rfc_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn websocket_accept_key_matches_rfc_6455() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn websocket_handshake_requires_upgrade_and_key() {
        let headers = vec![
            ("Upgrade".to_string(), "websocket".to_string()),
            ("Connection".to_string(), "keep-alive, Upgrade".to_string()),
            ("Sec-WebSocket-Version".to_string(), "13".to_string()),
            (
                "Sec-WebSocket-Key".to_string(),
                "dGhlIHNhbXBsZSBub25jZQ==".to_string(),
            ),
        ];
        assert!(ws_accept(&headers).is_ok());
        let mut bad = headers.clone();
        bad.retain(|(key, _)| key != "Sec-WebSocket-Key");
        assert!(ws_accept(&bad).is_err());
    }
}
