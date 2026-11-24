//! HTTP primitives shared by the management API, plugin UI and `/mcp`.

use serde_json::json;

pub use super::date::{http_date, parse_http_date};

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: Vec<(&'a str, &'a str)>,
}

/// An owned request head. The body is framed separately so callers can either
/// buffer it (management JSON) or stream it (blob uploads/downloads).
pub struct Head {
    pub method: String,
    pub path: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

/// Index of the `\r\n\r\n` that ends an HTTP request head.
pub fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Parse a complete request head (headers only, no body). Returns the owned
/// head and the byte offset where the body starts.
pub fn parse_head(bytes: &[u8]) -> Option<(Head, usize)> {
    let end = header_end(bytes)?;
    let request = parse(&bytes[..end + 4])?;
    let version = std::str::from_utf8(&bytes[..end])
        .ok()?
        .lines()
        .next()?
        .split_whitespace()
        .nth(2)
        .unwrap_or("HTTP/1.1")
        .to_string();
    Some((
        Head {
            method: request.method.to_string(),
            path: request.path.to_string(),
            version,
            headers: request
                .headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        },
        end + 4,
    ))
}

/// The interim response a server writes when it accepts a body that the
/// client offered with `Expect: 100-continue`.
pub const CONTINUE_RESPONSE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Whether this request asks the server to confirm the body first. Only
/// HTTP/1.1 defines the expectation; a declared body on a route that does not
/// read one must be refused without sending this interim response.
pub fn expects_continue(head: &Head) -> bool {
    head.version.eq_ignore_ascii_case("HTTP/1.1")
        && header(&head.headers, "expect")
            .map(|value| value.eq_ignore_ascii_case("100-continue"))
            .unwrap_or(false)
}

/// How the body of a request (or response) is delimited on the wire.
#[derive(Clone, Copy)]
pub enum Framing {
    None,
    Length(u64),
    Chunked,
}

/// Resolve `Content-Length` / `Transfer-Encoding` into one framing. Ambiguous
/// combinations are rejected: a request must not carry both, and only the
/// `chunked` transfer coding is supported.
pub fn framing(headers: &[(String, String)]) -> Result<Framing, String> {
    let mut lengths = Vec::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            for token in value.split(',') {
                let parsed = token
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| "invalid Content-Length".to_string())?;
                lengths.push(parsed);
            }
        }
    }
    let has_transfer_encoding = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"));
    if has_transfer_encoding {
        if !lengths.is_empty() {
            return Err("request has both Transfer-Encoding and Content-Length".into());
        }
        let mut codings = Vec::new();
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("transfer-encoding") {
                for token in value.split(',') {
                    let token = token.trim();
                    if !token.is_empty() {
                        codings.push(token.to_ascii_lowercase());
                    }
                }
            }
        }
        if codings.len() == 1 && codings[0] == "chunked" {
            return Ok(Framing::Chunked);
        }
        return Err("unsupported Transfer-Encoding".into());
    }
    match lengths.first() {
        None => Ok(Framing::None),
        Some(first) if lengths.iter().all(|length| length == first) => Ok(Framing::Length(*first)),
        Some(_) => Err("conflicting Content-Length headers".into()),
    }
}

/// Why a request body was refused while it was being decoded (#151).
///
/// The codec knows the two outcomes: malformed framing is the client's 400, a
/// limit overrun is 413. Every projection reads the variant, never the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyError {
    Malformed(String),
    TooLarge(String),
}

impl BodyError {
    pub fn status(&self) -> u16 {
        match self {
            BodyError::Malformed(_) => 400,
            BodyError::TooLarge(_) => 413,
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            BodyError::Malformed(detail) | BodyError::TooLarge(detail) => detail,
        }
    }
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.detail())
    }
}

/// Incremental decoder for `Transfer-Encoding: chunked` bodies. Input can be
/// split at any byte boundary; decoded bytes are handed to `sink` without
/// accumulating the whole body.
pub struct ChunkedDecoder {
    state: ChunkState,
    /// Only a partial size line or trailer line lives here; chunk data is
    /// passed straight from the caller's input slice to the sink.
    buffer: Vec<u8>,
    pending_cr: bool,
    remaining: u64,
    total: u64,
    max: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    Size,
    Data,
    DataCrlf,
    Trailer,
    Done,
}

impl ChunkedDecoder {
    pub fn new(max: u64) -> Self {
        ChunkedDecoder {
            state: ChunkState::Size,
            buffer: Vec::new(),
            pending_cr: false,
            remaining: 0,
            total: 0,
            max,
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == ChunkState::Done
    }

    pub fn feed<F>(&mut self, mut input: &[u8], sink: &mut F) -> Result<(), BodyError>
    where
        F: FnMut(&[u8]) -> Result<(), String> + ?Sized,
    {
        loop {
            match self.state {
                ChunkState::Size => {
                    if let Some(position) = find_crlf(&self.buffer) {
                        let line = self.buffer[..position].to_vec();
                        self.buffer.drain(..position + 2);
                        self.apply_size(&line)?;
                        continue;
                    }
                    if input.is_empty() {
                        return Ok(());
                    }
                    if self.buffer.last() == Some(&b'\r') && input.first() == Some(&b'\n') {
                        self.buffer.push(b'\n');
                        input = &input[1..];
                        let line = std::mem::take(&mut self.buffer);
                        self.apply_size(&line)?;
                        continue;
                    }
                    if let Some(position) = find_crlf(input) {
                        if self.buffer.len() + position > 8 * 1024 {
                            return Err(BodyError::Malformed("invalid chunk size line".into()));
                        }
                        self.buffer.extend_from_slice(&input[..position]);
                        input = &input[position + 2..];
                        let line = std::mem::take(&mut self.buffer);
                        self.apply_size(&line)?;
                        continue;
                    }
                    if self.buffer.len() + input.len() > 8 * 1024 {
                        return Err(BodyError::Malformed("invalid chunk size line".into()));
                    }
                    self.buffer.extend_from_slice(input);
                    return Ok(());
                }
                ChunkState::Data => {
                    if input.is_empty() {
                        return Ok(());
                    }
                    let take = std::cmp::min(input.len() as u64, self.remaining) as usize;
                    if self.total.saturating_add(take as u64) > self.max {
                        return Err(BodyError::TooLarge("request body is too large".into()));
                    }
                    sink(&input[..take]).map_err(BodyError::Malformed)?;
                    self.total += take as u64;
                    self.remaining -= take as u64;
                    input = &input[take..];
                    if self.remaining == 0 {
                        self.state = ChunkState::DataCrlf;
                    }
                }
                ChunkState::DataCrlf => {
                    if self.pending_cr {
                        if input.is_empty() {
                            return Ok(());
                        }
                        if input[0] != b'\n' {
                            return Err(BodyError::Malformed("invalid chunk terminator".into()));
                        }
                        input = &input[1..];
                        self.pending_cr = false;
                        self.state = ChunkState::Size;
                        continue;
                    }
                    if input.is_empty() {
                        return Ok(());
                    }
                    if input[0] != b'\r' {
                        return Err(BodyError::Malformed("invalid chunk terminator".into()));
                    }
                    if input.len() == 1 {
                        self.pending_cr = true;
                        return Ok(());
                    }
                    if input[1] != b'\n' {
                        return Err(BodyError::Malformed("invalid chunk terminator".into()));
                    }
                    input = &input[2..];
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer => {
                    if let Some(position) = find_crlf(&self.buffer) {
                        let empty = position == 0;
                        self.buffer.drain(..position + 2);
                        if empty {
                            self.state = ChunkState::Done;
                            self.buffer.clear();
                            return Ok(());
                        }
                        continue;
                    }
                    if input.is_empty() {
                        return Ok(());
                    }
                    if self.buffer.last() == Some(&b'\r') && input.first() == Some(&b'\n') {
                        self.buffer.push(b'\n');
                        input = &input[1..];
                        continue;
                    }
                    if let Some(position) = find_crlf(input) {
                        let empty = self.buffer.is_empty() && position == 0;
                        self.buffer.extend_from_slice(&input[..position]);
                        input = &input[position + 2..];
                        if empty {
                            self.state = ChunkState::Done;
                            self.buffer.clear();
                            return Ok(());
                        }
                        self.buffer.clear();
                        continue;
                    }
                    if self.buffer.len() + input.len() > 8 * 1024 {
                        return Err(BodyError::TooLarge("chunked trailer is too large".into()));
                    }
                    self.buffer.extend_from_slice(input);
                    return Ok(());
                }
                ChunkState::Done => return Ok(()),
            }
        }
    }

    fn apply_size(&mut self, line: &[u8]) -> Result<(), BodyError> {
        let size_text = line.split(|byte| *byte == b';').next().unwrap_or(line);
        let size_text = std::str::from_utf8(size_text)
            .map_err(|_| BodyError::Malformed("invalid chunk size".into()))?
            .trim();
        if size_text.is_empty() || size_text.len() > 16 {
            return Err(BodyError::Malformed("invalid chunk size".into()));
        }
        let size = u64::from_str_radix(size_text, 16)
            .map_err(|_| BodyError::Malformed("invalid chunk size".into()))?;
        if size == 0 {
            self.state = ChunkState::Trailer;
        } else {
            self.remaining = size;
            self.state = ChunkState::Data;
        }
        Ok(())
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}

/// Percent-decode one URL component. `plus_as_space` is set for query values;
/// path segments keep a literal `+`.
pub fn percent_decode(value: &str, plus_as_space: bool) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if let (Some(&high), Some(&low)) = (bytes.get(index + 1), bytes.get(index + 2)) {
                if let (Some(high), Some(low)) = (hex_value(high), hex_value(low)) {
                    out.push(high * 16 + low);
                    index += 3;
                    continue;
                }
            }
        }
        if plus_as_space && bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn parse(buf: &[u8]) -> Option<Request<'_>> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim(), v.trim()));
        }
    }
    Some(Request {
        method,
        path,
        headers,
    })
}

pub fn response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    response_with_headers(status, content_type, &[], body)
}

pub fn response_with_headers(
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = response_head(status, content_type, headers, body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// Whether a request method is HEAD. A HEAD response carries the same head as
/// the matching GET, including its `Content-Length`, but no body (RFC 9110
/// §9.3.2). The server's in-memory write points use this; a stream handler
/// that writes its own bytes keeps the same rule itself.
pub fn is_head(method: &str) -> bool {
    method.eq_ignore_ascii_case("HEAD")
}

/// Truncate a rendered response to its head. The declared `Content-Length` is
/// kept, so the bytes match what the same route would answer to GET.
pub fn strip_body(mut response: Vec<u8>) -> Vec<u8> {
    if let Some(end) = header_end(&response) {
        response.truncate(end + 4);
    }
    response
}

/// Status line plus headers for a body that the caller will write itself. Used
/// for streamed blob downloads and `HEAD`.
pub fn response_head(
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
    content_length: u64,
) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        303 => "See Other",
        304 => "Not Modified",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let mut out = Vec::with_capacity(160 + headers.len() * 32);
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            status, reason, content_type, content_length
        )
        .as_bytes(),
    );
    for (name, value) in headers {
        out.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Case-insensitive lookup over the owned header pairs an HTTP request
/// carries. `http::Request` keeps borrowed pairs, so it has its own lookup.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Methods that mutate state. Cookie-authenticated writes require the CSRF
/// header; the transport and every face use one definition.
pub fn is_write_method(method: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
}

/// The cookie-write CSRF policy: same-origin `Origin` plus `X-Svcd-Csrf: 1`.
/// A missing Host or Origin fails closed.
pub fn csrf_ok(headers: &[(String, String)]) -> bool {
    let host = header(headers, "host").unwrap_or("");
    let origin = header(headers, "origin").unwrap_or("");
    let csrf = header(headers, "x-svcd-csrf").unwrap_or("");
    !host.is_empty() && origin == format!("http://{}", host) && csrf == "1"
}

/// Same-origin check shared by the UI `/rpc` bridge, cookie writes and
/// `/mcp`. A missing `Origin` is allowed: non-browser clients do not send one
/// and the cookie surfaces require it explicitly (see `csrf_ok`).
pub fn same_origin(headers: &[(String, String)]) -> bool {
    let Some(origin) = header(headers, "origin") else {
        return true;
    };
    let host = header(headers, "host").unwrap_or("");
    if host.is_empty() {
        return false;
    }
    origin
        .trim()
        .strip_prefix("http://")
        .or_else(|| origin.trim().strip_prefix("https://"))
        .map(|origin_host| origin_host.eq_ignore_ascii_case(host))
        .unwrap_or(false)
}

/// The `{"error":{"code","message"}}` envelope every non-MCP HTTP error uses.
/// The stable error code a bare status maps to, for the drivers that render a
/// refusal before any face sees the request.
pub(crate) fn code_for(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        413 => "payload_too_large",
        431 => "headers_too_large",
        504 => "timeout",
        _ => "internal",
    }
}

pub fn json_error(status: u16, code: &str, message: &str) -> Vec<u8> {
    let body = json!({ "error": { "code": code, "message": message } });
    response(
        status,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

pub fn method_not_allowed(allow: &str) -> Vec<u8> {
    let body = json!({
        "error": {
            "code": "method_not_allowed",
            "message": format!("method not allowed; use {}", allow),
        }
    });
    response_with_headers(
        405,
        "application/json",
        &[("Allow".to_string(), allow.to_string())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// MIME by file extension. Unknown extensions fall back to
/// `application/octet-stream` so an unfamiliar file is never interpreted as a
/// document (the UI also sends `X-Content-Type-Options: nosniff`).
pub fn content_type(path: &str) -> &'static str {
    super::media::for_path(path).mime
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_headers_get_the_rfc_9110_status_and_code() {
        assert_eq!(code_for(431), "headers_too_large");
        let head = String::from_utf8(response(431, "application/json", b"{}")).expect("utf8");
        assert!(
            head.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"),
            "{head}"
        );
        assert!(head.contains("Connection: close\r\n"), "{head}");
    }

    #[test]
    fn content_type_is_chosen_by_extension() {
        for (path, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("INDEX.HTM", "text/html; charset=utf-8"),
            ("app.js", "text/javascript"),
            ("app.mjs", "text/javascript"),
            ("style.css", "text/css"),
            ("data.json", "application/json"),
            ("app.js.map", "application/json"),
            ("logo.svg", "image/svg+xml"),
            ("icon.png", "image/png"),
            ("photo.jpeg", "image/jpeg"),
            ("anim.gif", "image/gif"),
            ("image.webp", "image/webp"),
            ("photo.avif", "image/avif"),
            ("favicon.ico", "image/x-icon"),
            ("app.wasm", "application/wasm"),
            ("font.woff2", "font/woff2"),
            ("font.ttf", "font/ttf"),
            ("readme.txt", "text/plain; charset=utf-8"),
            ("feed.xml", "application/xml"),
            ("doc.pdf", "application/pdf"),
            ("payload.bin", "application/octet-stream"),
            ("no_extension", "application/octet-stream"),
        ] {
            assert_eq!(content_type(path), expected, "{path}");
        }
    }

    #[test]
    fn framing_accepts_length_and_chunked_but_rejects_ambiguity() {
        let headers = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<Vec<_>>()
        };
        assert!(matches!(
            framing(&headers(&[("Content-Length", "12")])),
            Ok(Framing::Length(12))
        ));
        assert!(matches!(
            framing(&headers(&[("Transfer-Encoding", "chunked")])),
            Ok(Framing::Chunked)
        ));
        assert!(matches!(framing(&headers(&[])), Ok(Framing::None)));
        assert!(framing(&headers(&[
            ("Content-Length", "12"),
            ("Transfer-Encoding", "chunked")
        ]))
        .is_err());
        assert!(framing(&headers(&[("Transfer-Encoding", "gzip")])).is_err());
        assert!(framing(&headers(&[("Content-Length", "aa")])).is_err());
        assert!(framing(&headers(&[
            ("Content-Length", "1"),
            ("Content-Length", "2")
        ]))
        .is_err());
    }

    #[test]
    fn chunked_decoder_handles_split_input_and_trailers() {
        let mut decoder = ChunkedDecoder::new(1024);
        let mut body = Vec::new();
        let wire = b"5\r\nhello\r\n6\r\n world\r\n0\r\nX-Checksum: ok\r\n\r\n";
        for window in wire.chunks(3) {
            decoder
                .feed(window, &mut |chunk| {
                    body.extend_from_slice(chunk);
                    Ok(())
                })
                .expect("chunk");
        }
        assert!(decoder.is_done());
        assert_eq!(body, b"hello world");
    }

    #[test]
    fn chunked_decoder_rejects_oversized_and_malformed_bodies() {
        let mut decoder = ChunkedDecoder::new(4);
        assert!(decoder
            .feed(b"5\r\nhello\r\n0\r\n\r\n", &mut |_| Ok(()))
            .is_err());
        let mut decoder = ChunkedDecoder::new(1024);
        assert!(decoder.feed(b"zz\r\n", &mut |_| Ok(())).is_err());
        let mut decoder = ChunkedDecoder::new(1024);
        assert!(decoder.feed(b"5\r\nhelloX\r\n", &mut |_| Ok(())).is_err());
    }

    #[test]
    fn chunked_decoder_streams_a_large_body_across_arbitrary_splits() {
        let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
        let mut wire = Vec::new();
        for chunk in payload.chunks(64 * 1024) {
            wire.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            wire.extend_from_slice(chunk);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\n\r\n");

        let mut decoder = ChunkedDecoder::new(2 * 1024 * 1024);
        let mut decoded = Vec::new();
        for part in wire.chunks(12_345) {
            decoder
                .feed(part, &mut |chunk| {
                    decoded.extend_from_slice(chunk);
                    Ok(())
                })
                .expect("chunk");
        }
        assert!(decoder.is_done());
        assert_eq!(decoded, payload);
    }

    #[test]
    fn percent_decode_keeps_plus_in_paths() {
        assert_eq!(percent_decode("sha256%3Aabcd", false), "sha256:abcd");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("a+b", true), "a b");
    }

    #[test]
    fn strip_body_keeps_the_head_and_its_content_length() {
        let full = response(401, "application/json", br#"{"error":"unauthorized"}"#);
        let head = strip_body(full.clone());
        let end = full
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("header end")
            + 4;
        assert_eq!(head, full[..end]);
        let text = String::from_utf8(head).expect("head is utf8");
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized"), "{text}");
        assert!(text.contains("Content-Length: 24"), "{text}");
        assert!(text.ends_with("\r\n\r\n"), "{text}");
        assert!(is_head("HEAD") && is_head("head") && !is_head("GET"));
        assert_eq!(strip_body(b"no header block".to_vec()), b"no header block");
    }
}
