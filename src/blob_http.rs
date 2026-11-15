//! Streaming HTTP surface for `/api/v1/blobs`.
//!
//! A blob upload is written straight to a temp file while its SHA-256 is
//! computed, and a download is streamed in chunks. This keeps daemon memory
//! independent of file size and is also where `Range`, `ETag` and conditional
//! requests live. Resumable uploads use a small S3-style part session:
//! `POST /uploads` creates it, `PUT /uploads/{id}/parts/{n}` writes one part
//! (retry by re-uploading the same number), and `POST .../complete` assembles
//! the selected parts into one content-addressed blob.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::blob;
use crate::http;
use crate::lan::{AuthKind, Lan};

const MAX_STREAMS: usize = 64;
const MULTIPART_OVERHEAD: u64 = 8 * 1024 * 1024;
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);

static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Which streaming route a request head selects.
pub enum Route {
    Upload,
    Part { upload: String, part: u32 },
    Download { id: String, head_only: bool },
    FileDownload { id: String, head_only: bool },
}

/// Select only the routes that need streaming. Session metadata (status,
/// complete, abort) stays on the ordinary buffered router.
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
        ("PUT", ["api", "v1", "blobs", "uploads", upload, "parts", part])
        | ("PUT", ["api", "v1", "file", "uploads", upload, "parts", part]) => Some(Route::Part {
            upload: http::percent_decode(upload, false),
            part: part.parse::<u32>().unwrap_or(0),
        }),
        ("GET", ["api", "v1", "blobs", id]) | ("HEAD", ["api", "v1", "blobs", id]) => {
            Some(Route::Download {
                id: http::percent_decode(id, false),
                head_only: head.method == "HEAD",
            })
        }
        ("GET", ["api", "v1", "file", id]) | ("HEAD", ["api", "v1", "file", id]) => {
            Some(Route::FileDownload {
                id: http::percent_decode(id, false),
                head_only: head.method == "HEAD",
            })
        }
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
    let _ = stream.set_read_timeout(Some(TRANSFER_TIMEOUT));
    let _ = stream.set_write_timeout(Some(TRANSFER_TIMEOUT));
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
        Route::Part { upload, part } => upload_part(stream, head, initial, &upload, part),
        Route::Download { id, head_only } => download(stream, head, &id, head_only),
        Route::FileDownload { id, head_only } => file_download(stream, head, &id, head_only),
    }
}

struct ActiveGuard;

impl ActiveGuard {
    fn acquire() -> Option<ActiveGuard> {
        if ACTIVE.fetch_add(1, Ordering::SeqCst) >= MAX_STREAMS {
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
        Ok(()) => match upload.finish() {
            Ok(meta) => {
                let body = serde_json::to_vec(&json!({"id": meta.id, "size": meta.size}))
                    .unwrap_or_default();
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
        },
        Err(message) => write_body_error(stream, &message),
    }
}

fn upload_part(
    stream: &mut TcpStream,
    head: &http::Head,
    initial: &[u8],
    upload_id: &str,
    part: u32,
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

    let mut writer = match blob::PartWriter::begin(upload_id, part) {
        Ok(writer) => writer,
        Err(message) => {
            let status = if message.contains("unknown upload") {
                404
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
        Ok(_) => match writer.finish() {
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
        },
        Err(message) => write_body_error(stream, &message),
    }
}

enum DownloadDecision {
    NotModified,
    PreconditionFailed,
    Unsatisfiable,
    Full,
    Range(u64, u64),
}

/// Evaluate the RFC 9110 preconditions and `Range`/`If-Range` for a GET/HEAD.
fn decide_download(
    head: &http::Head,
    id: &str,
    modified: SystemTime,
    total: u64,
) -> DownloadDecision {
    if let Some(value) = request_header(head, "if-match") {
        if !etag_matches(value, id, true) {
            return DownloadDecision::PreconditionFailed;
        }
    } else if let Some(date) =
        request_header(head, "if-unmodified-since").and_then(http::parse_http_date)
    {
        if modified > date + Duration::from_secs(1) {
            return DownloadDecision::PreconditionFailed;
        }
    }
    if let Some(value) = request_header(head, "if-none-match") {
        if etag_matches(value, id, false) {
            return DownloadDecision::NotModified;
        }
    } else if let Some(date) =
        request_header(head, "if-modified-since").and_then(http::parse_http_date)
    {
        if modified <= date + Duration::from_secs(1) {
            return DownloadDecision::NotModified;
        }
    }
    if let Some(value) = request_header(head, "range") {
        let allow = match request_header(head, "if-range") {
            None => true,
            Some(if_range) => match http::parse_http_date(if_range) {
                Some(date) => modified <= date + Duration::from_secs(1),
                None => etag_matches(if_range, id, true),
            },
        };
        if allow {
            match parse_range(value, total) {
                RangeResult::None => {}
                RangeResult::Range(start, end) => return DownloadDecision::Range(start, end),
                RangeResult::Unsatisfiable => return DownloadDecision::Unsatisfiable,
            }
        }
    }
    DownloadDecision::Full
}

fn write_range_error(
    stream: &mut TcpStream,
    head_only: bool,
    headers: &[(String, String)],
    total: u64,
) -> io::Result<()> {
    let body = serde_json::to_vec(&json!({
        "error": {
            "code": "range_not_satisfiable",
            "message": "range is not satisfiable",
        }
    }))
    .unwrap_or_default();
    let mut headers = headers.to_vec();
    headers.push(("Content-Range".to_string(), format!("bytes */{}", total)));
    if head_only {
        stream.write_all(&http::response_head(
            416,
            "application/json",
            &headers,
            body.len() as u64,
        ))
    } else {
        stream.write_all(&http::response_with_headers(
            416,
            "application/json",
            &headers,
            &body,
        ))
    }
}

fn download(
    stream: &mut TcpStream,
    head: &http::Head,
    id: &str,
    head_only: bool,
) -> io::Result<()> {
    let path = match blob::locate(id) {
        Ok(path) => path,
        Err(_) => return write_not_found(stream, head_only),
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return write_not_found(stream, head_only),
    };
    let total = metadata.len();
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    // Refresh the temp-cleanup clock after taking the stable metadata snapshot:
    // `Last-Modified` should describe the blob, not this particular read.
    blob::touch(&path);
    let quoted = format!("\"{}\"", id);
    let base_headers = vec![
        ("ETag".to_string(), quoted.clone()),
        ("Last-Modified".to_string(), http::http_date(modified)),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
    ];

    let mut range = None;
    match decide_download(head, id, modified, total) {
        DownloadDecision::NotModified => {
            return stream.write_all(&http::response_with_headers(
                304,
                "application/octet-stream",
                &base_headers,
                b"",
            ))
        }
        DownloadDecision::PreconditionFailed => {
            return write_conditional_error(
                stream,
                head_only,
                412,
                "precondition_failed",
                "precondition failed",
            )
        }
        DownloadDecision::Unsatisfiable => {
            return write_range_error(stream, head_only, &base_headers, total)
        }
        DownloadDecision::Full => {}
        DownloadDecision::Range(start, end) => range = Some((start, end)),
    }

    let (status, start, length, extra) = match range {
        Some((start, end)) => (
            206,
            start,
            end - start + 1,
            vec![(
                "Content-Range".to_string(),
                format!("bytes {}-{}/{}", start, end, total),
            )],
        ),
        None => (200, 0, total, Vec::new()),
    };
    // Open before writing the response head: if cleanup raced us, a 404 is
    // still possible instead of a truncated 200.
    let mut file = if head_only {
        None
    } else {
        match File::open(&path) {
            Ok(file) => Some(file),
            Err(_) => return write_not_found(stream, false),
        }
    };
    let mut headers = base_headers;
    headers.extend(extra);
    stream.write_all(&http::response_head(
        status,
        "application/octet-stream",
        &headers,
        length,
    ))?;
    if head_only {
        return stream.flush();
    }
    let Some(file) = file.as_mut() else {
        return stream.flush();
    };
    if start > 0 {
        file.seek(SeekFrom::Start(start))?;
    }
    let mut remaining = length;
    let mut buffer = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = std::cmp::min(buffer.len() as u64, remaining) as usize;
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    stream.flush()
}

fn file_download(
    stream: &mut TcpStream,
    head: &http::Head,
    id: &str,
    head_only: bool,
) -> io::Result<()> {
    let (directory, manifest) = match blob::file_manifest(id) {
        Ok(file) => file,
        Err(_) => return write_not_found(stream, head_only),
    };
    let manifest_path = directory.join("manifest.json");
    let modified = fs::metadata(&manifest_path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    blob::touch(&manifest_path);
    let total = manifest.size;
    let content_type = if manifest.mime_type.is_empty() {
        "application/octet-stream"
    } else {
        manifest.mime_type.as_str()
    };
    let quoted = format!("\"{}\"", manifest.id);
    let mut base_headers = vec![
        ("ETag".to_string(), quoted),
        ("Last-Modified".to_string(), http::http_date(modified)),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
    ];
    if !manifest.name.is_empty() {
        base_headers.push((
            "Content-Disposition".to_string(),
            format!(
                "attachment; filename=\"{}\"",
                sanitize_filename(&manifest.name)
            ),
        ));
    }

    let mut range = None;
    match decide_download(head, id, modified, total) {
        DownloadDecision::NotModified => {
            return stream.write_all(&http::response_with_headers(
                304,
                content_type,
                &base_headers,
                b"",
            ))
        }
        DownloadDecision::PreconditionFailed => {
            return write_conditional_error(
                stream,
                head_only,
                412,
                "precondition_failed",
                "precondition failed",
            )
        }
        DownloadDecision::Unsatisfiable => {
            return write_range_error(stream, head_only, &base_headers, total)
        }
        DownloadDecision::Full => {}
        DownloadDecision::Range(start, end) => range = Some((start, end)),
    }

    let (status, start, length, extra) = match range {
        Some((start, end)) => (
            206,
            start,
            end - start + 1,
            vec![(
                "Content-Range".to_string(),
                format!("bytes {}-{}/{}", start, end, total),
            )],
        ),
        None => (200, 0, total, Vec::new()),
    };
    base_headers.extend(extra);
    stream.write_all(&http::response_head(
        status,
        content_type,
        &base_headers,
        length,
    ))?;
    if head_only {
        return stream.flush();
    }
    stream_file_range(stream, &directory, &manifest, start, length)
}

fn stream_file_range(
    stream: &mut TcpStream,
    directory: &Path,
    manifest: &blob::FileManifest,
    start: u64,
    length: u64,
) -> io::Result<()> {
    let end = start.saturating_add(length);
    let mut offset = 0u64;
    let mut remaining = length;
    let mut buffer = vec![0u8; 64 * 1024];
    for part in &manifest.parts {
        let part_start = offset;
        let part_end = offset.saturating_add(part.size);
        offset = part_end;
        if part_end <= start {
            continue;
        }
        if part_start >= end {
            break;
        }
        let local_start = start.saturating_sub(part_start);
        let local_end = std::cmp::min(end, part_end) - part_start;
        let want = local_end.saturating_sub(local_start);
        if want == 0 {
            continue;
        }
        let path = directory.join(format!("part-{}", part.number));
        let mut file = File::open(&path)?;
        file.seek(SeekFrom::Start(local_start))?;
        let mut take = want;
        while take > 0 {
            let request = std::cmp::min(buffer.len() as u64, take) as usize;
            let read = file.read(&mut buffer[..request])?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("part {} is truncated", part.number),
                ));
            }
            stream.write_all(&buffer[..read])?;
            take -= read as u64;
        }
        remaining = remaining.saturating_sub(want);
        if remaining == 0 {
            break;
        }
    }
    if remaining != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "file bundle is missing bytes",
        ));
    }
    stream.flush()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|character| !character.is_control() && *character != '"')
        .take(200)
        .collect()
}

enum RangeResult {
    None,
    Range(u64, u64),
    Unsatisfiable,
}

fn parse_range(value: &str, total: u64) -> RangeResult {
    let value = value.trim();
    let Some(rest) = value.strip_prefix("bytes=") else {
        return RangeResult::None;
    };
    let specs: Vec<&str> = rest
        .split(',')
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .collect();
    // Multiple ranges are legal HTTP but not useful for opaque blobs; ignore
    // the header and return the full representation.
    if specs.len() != 1 {
        return RangeResult::None;
    }
    let Some((first, last)) = specs[0].split_once('-') else {
        return RangeResult::None;
    };
    if first.is_empty() {
        let Ok(suffix) = last.parse::<u64>() else {
            return RangeResult::None;
        };
        if suffix == 0 || total == 0 {
            return RangeResult::Unsatisfiable;
        }
        let start = total.saturating_sub(suffix);
        return RangeResult::Range(start, total - 1);
    }
    let Ok(start) = first.parse::<u64>() else {
        return RangeResult::None;
    };
    if start >= total {
        return RangeResult::Unsatisfiable;
    }
    let end = if last.is_empty() {
        total - 1
    } else {
        match last.parse::<u64>() {
            Ok(end) => end.min(total - 1),
            Err(_) => return RangeResult::None,
        }
    };
    if end < start {
        return RangeResult::Unsatisfiable;
    }
    RangeResult::Range(start, end)
}

fn etag_matches(header: &str, etag: &str, strong: bool) -> bool {
    for candidate in header.split(',') {
        let candidate = candidate.trim();
        if candidate == "*" {
            return true;
        }
        let weak = candidate.starts_with("W/");
        if strong && weak {
            continue;
        }
        let value = candidate.strip_prefix("W/").unwrap_or(candidate);
        if value.trim().trim_matches('"') == etag {
            return true;
        }
    }
    false
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

fn write_not_found(stream: &mut TcpStream, head_only: bool) -> io::Result<()> {
    write_conditional_error(stream, head_only, 404, "not_found", "blob not found")
}

fn write_conditional_error(
    stream: &mut TcpStream,
    head_only: bool,
    status: u16,
    code: &str,
    message: &str,
) -> io::Result<()> {
    if head_only {
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
        self.buffer.extend_from_slice(chunk);
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
                        let keep = delimiter.len();
                        if self.buffer.len() > keep {
                            let take = self.buffer.len() - keep;
                            if self.capturing {
                                upload.write(&self.buffer[..take])?;
                            }
                            self.buffer.drain(..take);
                        }
                        return Ok(());
                    };
                    if self.buffer.len() < position + delimiter.len() + 2 {
                        return Ok(());
                    }
                    if self.capturing {
                        upload.write(&self.buffer[..position])?;
                        self.capturing = false;
                    }
                    self.buffer.drain(..position + delimiter.len());
                    if self.buffer[..2] == *b"--" {
                        self.done = true;
                        self.buffer.clear();
                        return Ok(());
                    }
                    if self.buffer[..2] == *b"\r\n" {
                        self.buffer.drain(..2);
                        self.state = MpState::Headers;
                    } else {
                        return Err("invalid multipart boundary".into());
                    }
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

// -- Session metadata endpoints (ordinary buffered JSON) --------------------

// -- Logical file session endpoints (ordinary buffered JSON) ----------------

pub fn file_upload_create() -> Vec<u8> {
    match blob::create_upload() {
        Ok(id) => {
            let body = serde_json::to_vec(&json!({
                "uploadId": id,
                "partsUrl": format!("/api/v1/file/uploads/{}/parts", id),
            }))
            .unwrap_or_default();
            http::response_with_headers(
                200,
                "application/json",
                &[(
                    "Location".to_string(),
                    format!("/api/v1/file/uploads/{}", id),
                )],
                &body,
            )
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn file_upload_status(id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::upload_parts(id) {
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
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn file_upload_complete(id: &str, body: &[u8]) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
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

pub fn file_upload_abort(id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::abort_upload(id) {
        Ok(()) => http::response(
            200,
            "application/json",
            &serde_json::to_vec(&json!({"ok": true})).unwrap_or_default(),
        ),
        Err(message) if message.contains("unknown upload") => {
            http::json_error(404, "not_found", &message)
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn create_upload() -> Vec<u8> {
    match blob::create_upload() {
        Ok(id) => {
            let body = serde_json::to_vec(&json!({
                "uploadId": id,
                "partsUrl": format!("/api/v1/blobs/uploads/{}/parts", id),
            }))
            .unwrap_or_default();
            http::response_with_headers(
                200,
                "application/json",
                &[(
                    "Location".to_string(),
                    format!("/api/v1/blobs/uploads/{}", id),
                )],
                &body,
            )
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn upload_status(id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::upload_parts(id) {
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
        Err(message) => http::json_error(500, "internal", &message),
    }
}

pub fn complete_upload(id: &str, body: &[u8]) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
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

pub fn abort_upload(id: &str) -> Vec<u8> {
    if !blob::valid_upload_id(id) {
        return http::json_error(400, "bad_request", "invalid upload id");
    }
    match blob::abort_upload(id) {
        Ok(()) => http::response(
            200,
            "application/json",
            &serde_json::to_vec(&json!({"ok": true})).unwrap_or_default(),
        ),
        Err(message) if message.contains("unknown upload") => {
            http::json_error(404, "not_found", &message)
        }
        Err(message) => http::json_error(500, "internal", &message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parser_handles_common_forms() {
        assert!(matches!(
            parse_range("bytes=2-5", 10),
            RangeResult::Range(2, 5)
        ));
        assert!(matches!(
            parse_range("bytes=2-", 10),
            RangeResult::Range(2, 9)
        ));
        assert!(matches!(
            parse_range("bytes=-3", 10),
            RangeResult::Range(7, 9)
        ));
        assert!(matches!(
            parse_range("bytes=8-99", 10),
            RangeResult::Range(8, 9)
        ));
        assert!(matches!(
            parse_range("bytes=10-", 10),
            RangeResult::Unsatisfiable
        ));
        assert!(matches!(
            parse_range("bytes=5-2", 10),
            RangeResult::Unsatisfiable
        ));
        assert!(matches!(parse_range("items=0-1", 10), RangeResult::None));
        assert!(matches!(
            parse_range("bytes=0-1,4-5", 10),
            RangeResult::None
        ));
    }

    #[test]
    fn etag_matching_respects_weak_and_strong_comparison() {
        assert!(etag_matches("\"sha256:abc\"", "sha256:abc", true));
        assert!(etag_matches("W/\"sha256:abc\"", "sha256:abc", false));
        assert!(!etag_matches("W/\"sha256:abc\"", "sha256:abc", true));
        assert!(etag_matches("*", "sha256:abc", true));
        assert!(etag_matches(
            "\"other\", \"sha256:abc\"",
            "sha256:abc",
            true
        ));
        assert!(!etag_matches("\"other\"", "sha256:abc", true));
    }

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
}
