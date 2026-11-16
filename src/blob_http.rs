//! Streaming HTTP surface for blob uploads.
//!
//! A blob upload is written straight to a temp file while its SHA-256 is
//! computed. This keeps daemon memory independent of file size. The download
//! side, including `Range`, `ETag` and conditional requests, moved to
//! `face::blob` (#118). Resumable uploads use a small S3-style part session:
//! `POST /uploads` creates it, `PUT /uploads/{id}/parts/{n}` writes one part
//! (retry by re-uploading the same number), and `POST .../complete` assembles
//! the selected parts into one content-addressed blob.

use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::blob;
use crate::http;
use crate::lan::{AuthKind, Lan};

const MULTIPART_OVERHEAD: u64 = 8 * 1024 * 1024;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Which streaming upload route a request head selects. Downloads are a
/// sans-IO face handler now (`face::blob`); session metadata (status, complete,
/// abort) stays on the ordinary buffered router.
pub enum Route {
    Upload,
    Part {
        upload: String,
        part: u32,
        kind: blob::UploadKind,
    },
}

/// Select only the upload routes that still need the blocking takeover. #119
/// moves them to stream handlers and deletes this module's socket path.
pub fn route(head: &http::Head) -> Option<Route> {
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(head.path.as_str());
    let mut parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.get(2) == Some(&"files") {
        parts[2] = "file";
    }
    match (head.method.as_str(), parts.as_slice()) {
        ("POST", ["api", "v1", "blobs"]) => Some(Route::Upload),
        ("PUT", ["api", "v1", "blobs", "uploads", upload, "parts", part]) => Some(Route::Part {
            upload: http::percent_decode(upload, false),
            part: part.parse::<u32>().unwrap_or(0),
            kind: blob::UploadKind::Blob,
        }),
        ("PUT", ["api", "v1", "file", "uploads", upload, "parts", part]) => Some(Route::Part {
            upload: http::percent_decode(upload, false),
            part: part.parse::<u32>().unwrap_or(0),
            kind: blob::UploadKind::File,
        }),
        _ => None,
    }
}

/// Serve one already-authenticated route on a blocking socket. The response
/// always closes the connection: a streamed body has no useful keep-alive.
pub fn serve(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    lan: &Arc<Mutex<Lan>>,
    route: Route,
) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(crate::limits::current().transfer_timeout));
    let _ = stream.set_write_timeout(Some(crate::limits::current().transfer_timeout));
    let Some(_active) = ActiveGuard::acquire() else {
        return write_error_resp(
            stream,
            head,
            429,
            "too_many_streams",
            "too many concurrent blob transfers",
        );
    };

    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&head.headers) {
            Ok(auth) => auth,
            Err(()) => {
                return write_error_resp(
                    stream,
                    head,
                    401,
                    "unauthorized",
                    "authentication required",
                )
            }
        },
        Err(_) => return write_error_resp(stream, head, 500, "internal", "LAN state unavailable"),
    };
    if auth == AuthKind::Cookie && is_write(&head.method) && !csrf_ok(head) {
        return write_error_resp(
            stream,
            head,
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        );
    }

    match route {
        Route::Upload => upload(stream, head, initial),
        Route::Part { upload, part, kind } => {
            upload_part(stream, head, initial, &upload, part, kind)
        }
    }
}

struct ActiveGuard;

impl ActiveGuard {
    fn acquire() -> Option<ActiveGuard> {
        if ACTIVE.fetch_add(1, Ordering::SeqCst) >= crate::limits::current().max_streams {
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            None
        } else {
            Some(ActiveGuard)
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

fn is_write(method: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
}

fn csrf_ok(head: &http::Head) -> bool {
    let host = http::header(&head.headers, "host").unwrap_or("");
    let origin = http::header(&head.headers, "origin").unwrap_or("");
    let csrf = http::header(&head.headers, "x-svcd-csrf").unwrap_or("");
    !host.is_empty() && origin == format!("http://{}", host) && csrf == "1"
}

fn upload(stream: &mut TcpStream, head: &http::Head, initial: &[u8]) -> io::Result<()> {
    let content_type = http::header(&head.headers, "content-type").unwrap_or("");
    let multipart = match multipart_boundary(content_type) {
        Ok(boundary) => boundary,
        Err(message) => return write_error(stream, 400, "bad_request", &message),
    };
    let framing = match http::framing(&head.headers) {
        Ok(framing) => framing,
        Err(message) => return write_error(stream, 400, "bad_request", &message),
    };
    let limit = if multipart.is_some() {
        blob::MAX_BLOB_BYTES + MULTIPART_OVERHEAD
    } else {
        blob::MAX_BLOB_BYTES
    };
    match framing {
        http::Framing::None | http::Framing::Length(0) => {
            return write_error(stream, 400, "bad_request", "empty blob body")
        }
        http::Framing::Length(length) if length > limit => {
            return write_error(
                stream,
                413,
                "payload_too_large",
                "request body is too large",
            )
        }
        _ => {}
    }
    expect_continue(stream, head)?;

    let mut upload = match blob::Upload::begin() {
        Ok(upload) => upload,
        Err(message) => return write_error(stream, 500, "internal", &message),
    };
    let result = match multipart {
        Some(boundary) => {
            let mut parser = MultipartParser::new(&boundary);
            let copied = {
                let mut sink = |chunk: &[u8]| parser.push(chunk, &mut upload);
                http::copy_body(stream, framing, limit, initial, &mut sink)
            };
            match copied {
                Ok(_) => parser.finish(),
                Err(message) => Err(message),
            }
        }
        None => {
            let copied = {
                let mut sink = |chunk: &[u8]| upload.write(chunk);
                http::copy_body(
                    stream,
                    framing,
                    blob::MAX_FILE_PART_BYTES,
                    initial,
                    &mut sink,
                )
            };
            copied.map(|_| ())
        }
    };
    match result {
        Ok(()) if upload.size() == 0 => write_error(stream, 400, "bad_request", "empty blob body"),
        Ok(()) => finish_upload(stream, upload),
        Err(message) => write_body_error(stream, &message),
    }
}

fn finish_upload(stream: &mut TcpStream, upload: blob::Upload) -> io::Result<()> {
    match upload.finish() {
        Ok(meta) => {
            let body =
                serde_json::to_vec(&json!({"id": meta.id, "size": meta.size})).unwrap_or_default();
            let headers = vec![
                ("ETag".to_string(), format!("\"{}\"", meta.id)),
                ("Location".to_string(), format!("/api/v1/blobs/{}", meta.id)),
                ("Cache-Control".to_string(), "no-store".to_string()),
            ];
            stream.write_all(&http::response_with_headers(
                200,
                "application/json",
                &headers,
                &body,
            ))
        }
        Err(message) => write_error(stream, 500, "internal", &message),
    }
}

fn upload_part(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    upload_id: &str,
    part: u32,
    kind: blob::UploadKind,
) -> io::Result<()> {
    if !blob::valid_upload_id(upload_id) {
        return write_error(stream, 400, "bad_request", "invalid upload id");
    }
    if part == 0 || part > blob::MAX_PART_NUMBER {
        return write_error(
            stream,
            400,
            "bad_request",
            "part number must be between 1 and 10000",
        );
    }
    let framing = match http::framing(&head.headers) {
        Ok(framing) => framing,
        Err(message) => return write_error(stream, 400, "bad_request", &message),
    };
    match framing {
        http::Framing::None | http::Framing::Length(0) => {
            return write_error(stream, 400, "bad_request", "empty part body")
        }
        http::Framing::Length(length) if length > blob::MAX_FILE_PART_BYTES => {
            return write_error(
                stream,
                413,
                "payload_too_large",
                "request body is too large",
            )
        }
        _ => {}
    }
    expect_continue(stream, head)?;

    let mut writer = match blob::PartWriter::begin(upload_id, part, kind) {
        Ok(writer) => writer,
        Err(message) => {
            let status = if message.contains("unknown upload") {
                404
            } else if message.contains("kind mismatch") || message.contains("kind") {
                409
            } else if message.contains("part number") || message.contains("invalid") {
                400
            } else {
                500
            };
            return write_error(stream, status, code_for(status), &message);
        }
    };
    let copied = {
        let mut sink = |chunk: &[u8]| writer.write(chunk);
        http::copy_body(
            stream,
            framing,
            blob::MAX_FILE_PART_BYTES,
            initial,
            &mut sink,
        )
    };
    match copied {
        Ok(_) if writer.size() == 0 => write_error(stream, 400, "bad_request", "empty part body"),
        Ok(_) => finish_part(stream, writer),
        Err(message) => write_body_error(stream, &message),
    }
}

fn finish_part(stream: &mut TcpStream, writer: blob::PartWriter) -> io::Result<()> {
    match writer.finish() {
        Ok(info) => {
            let body = serde_json::to_vec(&json!({
                "partNumber": info.number,
                "size": info.size,
                "etag": info.etag,
            }))
            .unwrap_or_default();
            let headers = vec![("ETag".to_string(), format!("\"{}\"", info.etag))];
            stream.write_all(&http::response_with_headers(
                200,
                "application/json",
                &headers,
                &body,
            ))
        }
        Err(message) => write_error(stream, 500, "internal", &message),
    }
}

fn request_header<'a>(head: &'a http::Head, name: &str) -> Option<&'a str> {
    http::header(&head.headers, name)
}

fn expect_continue(stream: &mut TcpStream, head: &http::Head) -> io::Result<()> {
    let expected = request_header(head, "expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false);
    if expected && head.version.eq_ignore_ascii_case("HTTP/1.1") {
        stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    Ok(())
}

fn write_error(stream: &mut TcpStream, status: u16, code: &str, message: &str) -> io::Result<()> {
    stream.write_all(&http::json_error(status, code, message))
}

/// HEAD responses must not carry a body even on errors; keep the equivalent
/// `Content-Length` so the response still describes the representation.
fn write_error_resp(
    stream: &mut TcpStream,
    head: &http::Head,
    status: u16,
    code: &str,
    message: &str,
) -> io::Result<()> {
    if head.method == "HEAD" {
        let body = serde_json::to_vec(&json!({
            "error": {"code": code, "message": message}
        }))
        .unwrap_or_default();
        stream.write_all(&http::response_head(
            status,
            "application/json",
            &[],
            body.len() as u64,
        ))
    } else {
        write_error(stream, status, code, message)
    }
}

fn write_body_error(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    let status = if message.contains("too large") {
        413
    } else if message.contains("invalid")
        || message.contains("truncated")
        || message.contains("multipart")
        || message.contains("chunk")
    {
        400
    } else {
        500
    };
    write_error(stream, status, code_for(status), message)
}

fn code_for(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        404 => "not_found",
        409 => "conflict",
        413 => "payload_too_large",
        500 => "internal",
        _ => "error",
    }
}

fn multipart_boundary(content_type: &str) -> Result<Option<String>, String> {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if media != "multipart/form-data" {
        return Ok(None);
    }
    for parameter in content_type.split(';').skip(1) {
        let Some((name, value)) = parameter.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("boundary") {
            let value = value.trim().trim_matches('"');
            if value.is_empty() || value.len() > 70 {
                return Err("invalid multipart boundary".into());
            }
            return Ok(Some(value.to_string()));
        }
    }
    Err("multipart boundary is missing".into())
}

/// Extract the first file part from a streaming `multipart/form-data` body.
/// Parts that have no `filename=` are skipped; later parts are drained but not
/// stored.
struct MultipartParser {
    boundary: Vec<u8>,
    state: MpState,
    buffer: Vec<u8>,
    found_file: bool,
    capturing: bool,
    done: bool,
}

enum MpState {
    Preamble,
    Headers,
    Body,
    Boundary,
}

impl MultipartParser {
    fn new(boundary: &str) -> MultipartParser {
        let mut marker = Vec::with_capacity(boundary.len() + 2);
        marker.extend_from_slice(b"--");
        marker.extend_from_slice(boundary.as_bytes());
        MultipartParser {
            boundary: marker,
            state: MpState::Preamble,
            buffer: Vec::new(),
            found_file: false,
            capturing: false,
            done: false,
        }
    }

    fn push(&mut self, chunk: &[u8], upload: &mut blob::Upload) -> Result<(), String> {
        if self.done {
            return Ok(());
        }
        // The common case is a large file body with no partial line in the
        // parser buffer: hand the chunk straight to the upload writer instead
        // of copying the whole 64 KiB into `self.buffer` first.
        if matches!(self.state, MpState::Body) && self.buffer.is_empty() {
            return self.push_body_direct(chunk, upload);
        }
        self.buffer.extend_from_slice(chunk);
        self.advance(upload)
    }

    fn push_body_direct(&mut self, chunk: &[u8], upload: &mut blob::Upload) -> Result<(), String> {
        let mut delimiter = Vec::with_capacity(self.boundary.len() + 2);
        delimiter.extend_from_slice(b"\r\n");
        delimiter.extend_from_slice(&self.boundary);
        if let Some(position) = find(chunk, &delimiter) {
            if self.capturing {
                upload.write(&chunk[..position])?;
                self.capturing = false;
            }
            let rest = &chunk[position + delimiter.len()..];
            self.buffer.extend_from_slice(rest);
            self.state = MpState::Boundary;
            return self.advance(upload);
        }
        let keep = keep_boundary_tail(chunk, &delimiter);
        let take = chunk.len() - keep;
        if self.capturing {
            upload.write(&chunk[..take])?;
        }
        self.buffer.extend_from_slice(&chunk[take..]);
        Ok(())
    }

    fn advance(&mut self, upload: &mut blob::Upload) -> Result<(), String> {
        loop {
            match self.state {
                MpState::Preamble => {
                    let Some(position) = find(&self.buffer, &self.boundary) else {
                        let keep = self.boundary.len() + 2;
                        if self.buffer.len() > keep {
                            let drop = self.buffer.len() - keep;
                            self.buffer.drain(..drop);
                        }
                        return Ok(());
                    };
                    self.buffer.drain(..position + self.boundary.len());
                    self.state = MpState::Headers;
                }
                MpState::Headers => {
                    let Some(position) = find(&self.buffer, b"\r\n\r\n") else {
                        if self.buffer.len() > 64 * 1024 {
                            return Err("multipart part headers are too large".into());
                        }
                        return Ok(());
                    };
                    let headers = self.buffer[..position].to_vec();
                    self.buffer.drain(..position + 4);
                    self.capturing = !self.found_file && is_file_part(&headers);
                    if self.capturing {
                        self.found_file = true;
                    }
                    self.state = MpState::Body;
                }
                MpState::Body => {
                    let mut delimiter = Vec::with_capacity(self.boundary.len() + 2);
                    delimiter.extend_from_slice(b"\r\n");
                    delimiter.extend_from_slice(&self.boundary);
                    let Some(position) = find(&self.buffer, &delimiter) else {
                        let keep = keep_boundary_tail(&self.buffer, &delimiter);
                        let take = self.buffer.len() - keep;
                        if take > 0 {
                            if self.capturing {
                                upload.write(&self.buffer[..take])?;
                            }
                            self.buffer.drain(..take);
                        }
                        return Ok(());
                    };
                    if self.capturing {
                        upload.write(&self.buffer[..position])?;
                        self.capturing = false;
                    }
                    self.buffer.drain(..position + delimiter.len());
                    self.state = MpState::Boundary;
                }
                MpState::Boundary => {
                    if self.buffer.len() < 2 {
                        return Ok(());
                    }
                    if self.buffer[..2] == *b"--" {
                        self.done = true;
                        self.buffer.clear();
                        return Ok(());
                    }
                    if self.buffer[..2] == *b"\r\n" {
                        self.buffer.drain(..2);
                        self.state = MpState::Headers;
                        continue;
                    }
                    return Err("invalid multipart boundary".into());
                }
            }
        }
    }

    fn finish(&self) -> Result<(), String> {
        if !self.found_file {
            return Err("multipart body has no file part".into());
        }
        if !self.done {
            return Err("incomplete multipart body".into());
        }
        Ok(())
    }
}

fn is_file_part(headers: &[u8]) -> bool {
    String::from_utf8_lossy(headers).lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("content-disposition:") && lower.contains("filename")
    })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// How many trailing bytes might be the beginning of `delimiter`? Keeping only
/// that suffix lets the next push process almost the whole chunk directly.
fn keep_boundary_tail(data: &[u8], delimiter: &[u8]) -> usize {
    let max = std::cmp::min(data.len(), delimiter.len().saturating_sub(1));
    for length in (1..=max).rev() {
        if data[data.len() - length..] == delimiter[..length] {
            return length;
        }
    }
    0
}

// -- Session metadata endpoints (ordinary buffered JSON) --------------------

// -- Logical file session endpoints (ordinary buffered JSON) ----------------

pub fn create_upload(kind: blob::UploadKind) -> Vec<u8> {
    match blob::create_upload(kind) {
        Ok(id) => {
            let prefix = match kind {
                blob::UploadKind::Blob => "blobs",
                blob::UploadKind::File => "file",
            };
            let body = serde_json::to_vec(&json!({
                "uploadId": id,
                "partsUrl": format!("/api/v1/{}/uploads/{}/parts", prefix, id),
            }))
            .unwrap_or_default();
            http::response_with_headers(
                200,
                "application/json",
                &[(
                    "Location".to_string(),
                    format!("/api/v1/{}/uploads/{}", prefix, id),
                )],
                &body,
            )
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn upload_status(kind: blob::UploadKind, id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::upload_parts(id, kind) {
        Ok(parts) => {
            let total: u64 = parts.iter().map(|part| part.size).sum();
            let parts: Vec<Value> = parts
                .iter()
                .map(|part| {
                    json!({
                        "partNumber": part.number,
                        "size": part.size,
                        "etag": part.etag,
                    })
                })
                .collect();
            let body = serde_json::to_vec(&json!({
                "uploadId": id,
                "parts": parts,
                "totalSize": total,
            }))
            .unwrap_or_default();
            http::response(200, "application/json", &body)
        }
        Err(message) if message.contains("unknown upload") => {
            http::json_error(404, "not_found", &message)
        }
        Err(message) if message.contains("kind mismatch") || message.contains("kind") => {
            http::json_error(409, "conflict", &message)
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn file_upload_complete(id: &str, body: &[u8]) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    if let Some(response) = kind_rejection(id, blob::UploadKind::File) {
        return response;
    }
    let mut requested = None;
    let mut name = None;
    let mut mime_type = None;
    if !body.is_empty() {
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return http::json_error(400, "bad_request", "body must be JSON"),
        };
        if let Some(parts) = value.get("parts") {
            let Some(parts) = parts.as_array() else {
                return http::json_error(400, "bad_request", "parts must be an array");
            };
            let mut list = Vec::with_capacity(parts.len());
            for part in parts {
                let Some(number) = part.get("partNumber").and_then(Value::as_u64) else {
                    return http::json_error(400, "bad_request", "partNumber is required");
                };
                if number == 0 || number > blob::MAX_PART_NUMBER as u64 {
                    return http::json_error(
                        400,
                        "bad_request",
                        "partNumber must be between 1 and 10000",
                    );
                }
                let etag = part.get("etag").and_then(Value::as_str).map(str::to_string);
                list.push((number as u32, etag));
            }
            requested = Some(list);
        }
        name = value
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        mime_type = value
            .get("mimeType")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if name
            .as_ref()
            .map(|value| value.len() > 255)
            .unwrap_or(false)
            || mime_type
                .as_ref()
                .map(|value| value.len() > 255)
                .unwrap_or(false)
        {
            return http::json_error(400, "bad_request", "name or mimeType is too long");
        }
        let header_safe = |value: &str| !value.chars().any(char::is_control);
        if name
            .as_deref()
            .map(|value| !header_safe(value))
            .unwrap_or(false)
            || mime_type
                .as_deref()
                .map(|value| !header_safe(value))
                .unwrap_or(false)
        {
            return http::json_error(
                400,
                "bad_request",
                "name or mimeType contains control characters",
            );
        }
    }
    match blob::complete_file(id, requested, name, mime_type) {
        Ok(meta) => {
            let body = serde_json::to_vec(&json!({
                "id": meta.id,
                "size": meta.size,
                "name": meta.name,
                "mimeType": meta.mime_type,
            }))
            .unwrap_or_default();
            let headers = vec![
                ("ETag".to_string(), format!("\"{}\"", meta.id)),
                ("Location".to_string(), format!("/api/v1/file/{}", meta.id)),
            ];
            http::response_with_headers(200, "application/json", &headers, &body)
        }
        Err(message) if message.contains("unknown upload") => {
            http::json_error(404, "not_found", &message)
        }
        Err(message) if message.contains("kind mismatch") || message.contains("kind") => {
            http::json_error(409, "conflict", &message)
        }
        Err(message) if message.contains("too large") => {
            http::json_error(413, "payload_too_large", &message)
        }
        Err(message)
            if message.contains("part")
                || message.contains("invalid")
                || message.contains("empty")
                || message.contains("changed") =>
        {
            http::json_error(400, "bad_request", &message)
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn abort_upload(kind: blob::UploadKind, id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::abort_upload(id, kind) {
        Ok(()) => http::response(
            200,
            "application/json",
            &serde_json::to_vec(&json!({"ok": true})).unwrap_or_default(),
        ),
        Err(message) if message.contains("unknown upload") => {
            http::json_error(404, "not_found", &message)
        }
        Err(message) if message.contains("kind mismatch") || message.contains("kind") => {
            http::json_error(409, "conflict", &message)
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

fn kind_rejection(id: &str, kind: blob::UploadKind) -> Option<Vec<u8>> {
    match blob::require_upload_kind(id, kind) {
        Ok(()) => None,
        Err(message) if message.contains("unknown upload") => {
            Some(http::json_error(404, "not_found", &message))
        }
        Err(message) if message.contains("kind") => {
            Some(http::json_error(409, "conflict", &message))
        }
        Err(message) => Some(http::json_error(500, "internal", &message)),
    }
}

pub fn complete_upload(id: &str, body: &[u8]) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    if let Some(response) = kind_rejection(id, blob::UploadKind::Blob) {
        return response;
    }
    let requested = if body.is_empty() {
        None
    } else {
        let value: Value = match serde_json::from_slice(body) {
            Ok(value) => value,
            Err(_) => return http::json_error(400, "bad_request", "body must be JSON"),
        };
        let Some(parts) = value.get("parts").and_then(Value::as_array) else {
            return http::json_error(400, "bad_request", "parts is required");
        };
        let mut requested = Vec::with_capacity(parts.len());
        for part in parts {
            let Some(number) = part.get("partNumber").and_then(Value::as_u64) else {
                return http::json_error(400, "bad_request", "partNumber is required");
            };
            if number == 0 || number > blob::MAX_PART_NUMBER as u64 {
                return http::json_error(
                    400,
                    "bad_request",
                    "partNumber must be between 1 and 10000",
                );
            }
            let etag = part.get("etag").and_then(Value::as_str).map(str::to_string);
            requested.push((number as u32, etag));
        }
        Some(requested)
    };
    match blob::complete_upload(id, requested) {
        Ok(meta) => {
            let body =
                serde_json::to_vec(&json!({"id": meta.id, "size": meta.size})).unwrap_or_default();
            let headers = vec![
                ("ETag".to_string(), format!("\"{}\"", meta.id)),
                ("Location".to_string(), format!("/api/v1/blobs/{}", meta.id)),
            ];
            http::response_with_headers(200, "application/json", &headers, &body)
        }
        Err(message) if message.contains("unknown upload") => {
            http::json_error(404, "not_found", &message)
        }
        Err(message) if message.contains("kind mismatch") || message.contains("kind") => {
            http::json_error(409, "conflict", &message)
        }
        Err(message) if message.contains("too large") => {
            http::json_error(413, "payload_too_large", &message)
        }
        Err(message)
            if message.contains("part")
                || message.contains("invalid")
                || message.contains("empty") =>
        {
            http::json_error(400, "bad_request", &message)
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multipart_boundary_is_parsed_from_content_type() {
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=abc123").expect("boundary"),
            Some("abc123".to_string())
        );
        assert_eq!(
            multipart_boundary("multipart/form-data; boundary=\"a b\"").expect("quoted"),
            Some("a b".to_string())
        );
        assert_eq!(
            multipart_boundary("application/octet-stream").expect("raw"),
            None
        );
        assert!(multipart_boundary("multipart/form-data").is_err());
    }

    fn head(method: &str, path: &str) -> http::Head {
        http::Head {
            method: method.to_string(),
            path: path.to_string(),
            version: "HTTP/1.1".to_string(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn downloads_never_enter_the_upload_takeover() {
        // The face owns downloads now; the takeover route table must not claim
        // a GET/HEAD. This is also the cross-platform proof that no
        // `svcd-blob-http` thread can be spawned for a download.
        for (method, path) in [
            ("GET", "/api/v1/blobs/sha256:abc"),
            ("HEAD", "/api/v1/blobs/sha256:abc"),
            ("GET", "/api/v1/file/sha256:abc"),
            ("GET", "/api/v1/files/sha256:abc"),
            ("HEAD", "/api/v1/file/sha256:abc?x=1"),
        ] {
            assert!(
                route(&head(method, path)).is_none(),
                "{method} {path} must not use the upload takeover"
            );
        }
        assert!(matches!(
            route(&head("POST", "/api/v1/blobs")),
            Some(Route::Upload)
        ));
        assert!(matches!(
            route(&head("PUT", "/api/v1/blobs/uploads/u/parts/1")),
            Some(Route::Part { .. })
        ));
    }
}
