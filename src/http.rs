//! HTTP primitives shared by the management API, plugin UI and `/mcp`.

#![allow(dead_code)]

use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

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

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn feed<F>(&mut self, mut input: &[u8], sink: &mut F) -> Result<(), String>
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
                            return Err("invalid chunk size line".into());
                        }
                        self.buffer.extend_from_slice(&input[..position]);
                        input = &input[position + 2..];
                        let line = std::mem::take(&mut self.buffer);
                        self.apply_size(&line)?;
                        continue;
                    }
                    if self.buffer.len() + input.len() > 8 * 1024 {
                        return Err("invalid chunk size line".into());
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
                        return Err("request body is too large".into());
                    }
                    sink(&input[..take])?;
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
                            return Err("invalid chunk terminator".into());
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
                        return Err("invalid chunk terminator".into());
                    }
                    if input.len() == 1 {
                        self.pending_cr = true;
                        return Ok(());
                    }
                    if input[1] != b'\n' {
                        return Err("invalid chunk terminator".into());
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
                        return Err("chunked trailer is too large".into());
                    }
                    self.buffer.extend_from_slice(input);
                    return Ok(());
                }
                ChunkState::Done => return Ok(()),
            }
        }
    }

    fn apply_size(&mut self, line: &[u8]) -> Result<(), String> {
        let size_text = line.split(|byte| *byte == b';').next().unwrap_or(line);
        let size_text = std::str::from_utf8(size_text)
            .map_err(|_| "invalid chunk size".to_string())?
            .trim();
        if size_text.is_empty() || size_text.len() > 16 {
            return Err("invalid chunk size".into());
        }
        let size =
            u64::from_str_radix(size_text, 16).map_err(|_| "invalid chunk size".to_string())?;
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

/// Stream a request body into `sink`. `initial` is whatever the caller read
/// after the head; any extra bytes beyond the declared length are ignored.
pub fn copy_body<R: Read>(
    reader: &mut R,
    framing: Framing,
    max: u64,
    initial: &[u8],
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<u64, String> {
    match framing {
        Framing::None => Ok(0),
        Framing::Length(length) => {
            if length > max {
                return Err("request body is too large".into());
            }
            let mut total = 0u64;
            let take = std::cmp::min(initial.len() as u64, length) as usize;
            if take > 0 {
                sink(&initial[..take])?;
                total += take as u64;
            }
            let mut buffer = [0u8; 64 * 1024];
            while total < length {
                let want = std::cmp::min(buffer.len() as u64, length - total) as usize;
                let read = reader
                    .read(&mut buffer[..want])
                    .map_err(|error| format!("read request body: {}", error))?;
                if read == 0 {
                    return Err("truncated request body".into());
                }
                sink(&buffer[..read])?;
                total += read as u64;
            }
            Ok(total)
        }
        Framing::Chunked => {
            let mut decoder = ChunkedDecoder::new(max);
            decoder.feed(initial, sink)?;
            let mut buffer = [0u8; 64 * 1024];
            while !decoder.is_done() {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|error| format!("read request body: {}", error))?;
                if read == 0 {
                    return Err("truncated chunked body".into());
                }
                decoder.feed(&buffer[..read], sink)?;
            }
            Ok(decoder.total())
        }
    }
}

/// Buffer a body in memory. Only for small management JSON requests; blobs are
/// streamed with `copy_body` instead.
pub fn collect_body<R: Read>(
    reader: &mut R,
    framing: Framing,
    max: u64,
    initial: &[u8],
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    copy_body(reader, framing, max, initial, &mut |chunk| {
        body.extend_from_slice(chunk);
        Ok(())
    })?;
    Ok(body)
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
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Format an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub fn http_date(time: SystemTime) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday; WEEKDAYS starts with Thursday.
    let weekday = days.rem_euclid(7) as usize;
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[weekday],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// Parse the IMF-fixdate form emitted by `http_date`. Other obsolete HTTP date
/// forms are ignored (callers then fall back to treating the condition as
/// absent), which is allowed by RFC 9110.
pub fn parse_http_date(text: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = text.trim().split_whitespace();
    let _weekday = parts.next()?;
    let day: u32 = parts.next()?.trim_end_matches(',').parse().ok()?;
    let month_name = parts.next()?;
    let month = MONTHS
        .iter()
        .position(|name| name.eq_ignore_ascii_case(month_name))? as u32
        + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    if !parts.next()?.eq_ignore_ascii_case("GMT") || parts.next().is_some() {
        return None;
    }
    if !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add((hour * 3600 + minute * 60 + second) as i64)?;
    if seconds < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to Y/M/D.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(decoder.total(), 11);
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
    fn copy_body_reads_length_and_chunked_from_initial_and_reader() {
        let mut reader = std::io::Cursor::new(b"world".to_vec());
        let mut body = Vec::new();
        copy_body(
            &mut reader,
            Framing::Length(11),
            64,
            b"hello ",
            &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            },
        )
        .expect("length");
        assert_eq!(body, b"hello world");

        let mut reader = std::io::Cursor::new(b" world\r\n0\r\n\r\n".to_vec());
        let mut body = Vec::new();
        copy_body(
            &mut reader,
            Framing::Chunked,
            64,
            b"5\r\nhello\r\n6\r\n",
            &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            },
        )
        .expect("chunked");
        assert_eq!(body, b"hello world");
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
    fn http_dates_round_trip() {
        for text in [
            "Thu, 01 Jan 1970 00:00:00 GMT",
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sun, 15 Nov 2026 12:00:00 GMT",
        ] {
            let parsed = parse_http_date(text).expect("date");
            assert_eq!(http_date(parsed), text);
        }
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST").is_none());
    }

    #[test]
    fn percent_decode_keeps_plus_in_paths() {
        assert_eq!(percent_decode("sha256%3Aabcd", false), "sha256:abcd");
        assert_eq!(percent_decode("a+b", false), "a+b");
        assert_eq!(percent_decode("a+b", true), "a b");
    }
}
