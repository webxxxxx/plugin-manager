//! `/api/v1/blobs/{id}` and `/api/v1/file/{id}` downloads as a sans-IO stream.
//!
//! Range, ETag, `If-*` and `If-Range` semantics are the ones the old blocking
//! takeover implemented; the wire bytes stay the same. The difference is where
//! the body comes from: the server performs one file send per range instead of
//! the face owning a socket and a transfer thread. A slow reader therefore
//! blocks its own connection (the kernel file send or the bounded output
//! buffer), never the daemon loop.

use std::fs::{self, File};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::blob;
use crate::http;
use crate::server::http::{
    CloseReason, FileSend, OutBuffer, Request, StreamAction, StreamClass, StreamEvent,
    StreamHandler, StreamStep,
};

/// One recognized download. Uploads and JSON session metadata stay on the
/// legacy path until #119.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadTarget {
    Blob { id: String, head_only: bool },
    File { id: String, head_only: bool },
}

impl DownloadTarget {
    fn id(&self) -> &str {
        match self {
            DownloadTarget::Blob { id, .. } | DownloadTarget::File { id, .. } => id,
        }
    }

    fn head_only(&self) -> bool {
        match self {
            DownloadTarget::Blob { head_only, .. } | DownloadTarget::File { head_only, .. } => {
                *head_only
            }
        }
    }
}

/// Recognize only GET/HEAD downloads. The path parser mirrors the old
/// takeover: /api/v1/files/{id} is the historical alias for
/// /api/v1/file/{id}.
pub fn download_target(method: &str, path: &str) -> Option<DownloadTarget> {
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    let mut parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.get(2) == Some(&"files") {
        parts[2] = "file";
    }
    let head_only = method == "HEAD";
    match (method, parts.as_slice()) {
        ("GET" | "HEAD", ["api", "v1", "blobs", id]) => Some(DownloadTarget::Blob {
            id: http::percent_decode(id, false),
            head_only,
        }),
        ("GET" | "HEAD", ["api", "v1", "file", id]) => Some(DownloadTarget::File {
            id: http::percent_decode(id, false),
            head_only,
        }),
        _ => None,
    }
}

/// A blob or logical-file download as a sans-IO state machine.
pub struct DownloadStream {
    /// Complete response head. Content-Length describes the representation,
    /// so a HEAD response carries it without a body.
    head: Vec<u8>,
    head_sent: usize,
    body: Body,
}

enum Body {
    File {
        file: File,
        offset: u64,
        length: u64,
    },
    Bundle {
        directory: PathBuf,
        parts: Vec<BundlePart>,
        index: usize,
        /// Absolute offset in the logical file of the next byte to send.
        position: u64,
        remaining: u64,
    },
    Done,
}

struct BundlePart {
    number: u32,
    start: u64,
    size: u64,
}

/// The head and body range a download decision selected.
struct DownloadPlan {
    head: Vec<u8>,
    start: u64,
    length: u64,
}

impl DownloadStream {
    /// Resolve one recognized download. Every failure is still a stream
    /// response: the route body is Streamed, so a memory response here would
    /// violate the S4 contract after headers.
    pub fn open(request: &Request, target: DownloadTarget) -> DownloadStream {
        match &target {
            DownloadTarget::Blob { .. } => blob_download(&request.headers, &target),
            DownloadTarget::File { .. } => file_download(&request.headers, &target),
        }
    }

    fn response(head: Vec<u8>) -> DownloadStream {
        DownloadStream {
            head,
            head_sent: 0,
            body: Body::Done,
        }
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        // Take the body out so a file handle can move into FileSend; put it
        // back whenever a bounded write leaves work for the next Writable.
        let body = std::mem::replace(&mut self.body, Body::Done);
        if self.head_sent < self.head.len() {
            let take = (self.head.len() - self.head_sent).min(out.remaining());
            let complete = out.write(&self.head[self.head_sent..self.head_sent + take]);
            debug_assert!(complete, "the head chunk was clamped to free capacity");
            self.head_sent += take;
            if self.head_sent < self.head.len() {
                self.body = body;
                return StreamStep::wait(StreamAction::NeedWrite);
            }
        }
        match body {
            Body::File {
                file,
                offset,
                length,
            } => {
                // The body was already replaced with Done; a second Writable
                // means the whole range is on the wire.
                StreamStep::wait(StreamAction::SendFile(FileSend {
                    file,
                    offset,
                    length,
                }))
            }
            Body::Bundle {
                directory,
                parts,
                mut index,
                mut position,
                mut remaining,
            } => loop {
                if remaining == 0 {
                    return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
                }
                let Some(part) = parts.get(index) else {
                    return StreamStep::wait(StreamAction::Close(CloseReason::Failed(
                        "file bundle is missing bytes".into(),
                    )));
                };
                let part_end = part.start.saturating_add(part.size);
                if position >= part_end {
                    index += 1;
                    continue;
                }
                let local_start = position - part.start;
                let want = (part_end - position).min(remaining);
                let path = directory.join(format!("part-{}", part.number));
                let file =
                    match File::open(&path) {
                        Ok(file) => file,
                        Err(error) => {
                            return StreamStep::wait(StreamAction::Close(CloseReason::Failed(
                                format!("open {}: {}", path.display(), error),
                            )))
                        }
                    };
                position += want;
                remaining -= want;
                if position >= part_end {
                    index += 1;
                }
                self.body = Body::Bundle {
                    directory,
                    parts,
                    index,
                    position,
                    remaining,
                };
                return StreamStep::wait(StreamAction::SendFile(FileSend {
                    file,
                    offset: local_start,
                    length: want,
                }));
            },
            Body::Done => StreamStep::wait(StreamAction::Close(CloseReason::Completed)),
        }
    }
}

impl StreamHandler for DownloadStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Tick(_) => StreamStep::wait(StreamAction::Close(CloseReason::TimedOut)),
            StreamEvent::Closed(reason) => {
                self.body = Body::Done;
                StreamStep::wait(StreamAction::Close(reason))
            }
            // A download has no request body and submits no blocking job.
            StreamEvent::Data(_) | StreamEvent::BlockingDone(_) => {
                StreamStep::wait(StreamAction::Continue)
            }
        }
    }

    fn stream_class(&self) -> StreamClass {
        StreamClass::LongLived
    }
}

fn blob_download(headers: &[(String, String)], target: &DownloadTarget) -> DownloadStream {
    let path = match blob::locate(target.id()) {
        Ok(path) => path,
        Err(_) => return not_found(target),
    };
    let metadata = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return not_found(target),
    };
    let total = metadata.len();
    let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
    // Refresh the temp-cleanup clock after taking the stable metadata snapshot:
    // Last-Modified should describe the blob, not this particular read.
    blob::touch(&path);
    let plan = match plan_download(
        headers,
        target,
        modified,
        total,
        "application/octet-stream",
        download_headers(target.id(), modified),
    ) {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    if target.head_only() {
        return DownloadStream::response(plan.head);
    }
    // Open before the server emits headers: if cleanup raced the metadata
    // snapshot, a 404 is still possible instead of a truncated 200.
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(_) => return not_found(target),
    };
    DownloadStream {
        head: plan.head,
        head_sent: 0,
        body: if plan.length == 0 {
            Body::Done
        } else {
            Body::File {
                file,
                offset: plan.start,
                length: plan.length,
            }
        },
    }
}

fn file_download(headers: &[(String, String)], target: &DownloadTarget) -> DownloadStream {
    let (directory, manifest) = match blob::file_manifest(target.id()) {
        Ok(file) => file,
        Err(_) => return not_found(target),
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
    let mut response_headers = download_headers(&manifest.id, modified);
    if !manifest.name.is_empty() {
        response_headers.push((
            "Content-Disposition".to_string(),
            format!(
                "attachment; filename=\"{}\"",
                sanitize_filename(&manifest.name)
            ),
        ));
    }
    let plan = match plan_download(
        headers,
        target,
        modified,
        total,
        content_type,
        response_headers,
    ) {
        Ok(plan) => plan,
        Err(response) => return response,
    };
    if target.head_only() || plan.length == 0 {
        return DownloadStream::response(plan.head);
    }
    let mut offset = 0u64;
    let parts: Vec<BundlePart> = manifest
        .parts
        .iter()
        .map(|part| {
            let start = offset;
            offset = offset.saturating_add(part.size);
            BundlePart {
                number: part.number,
                start,
                size: part.size,
            }
        })
        .collect();
    DownloadStream {
        head: plan.head,
        head_sent: 0,
        body: Body::Bundle {
            directory,
            parts,
            index: 0,
            position: plan.start,
            remaining: plan.length,
        },
    }
}

fn download_headers(id: &str, modified: SystemTime) -> Vec<(String, String)> {
    vec![
        ("ETag".to_string(), format!("\"{}\"", id)),
        ("Last-Modified".to_string(), http::http_date(modified)),
        ("Accept-Ranges".to_string(), "bytes".to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
    ]
}

/// Evaluate the preconditions and Range once, then build the response head.
/// A 304/412/416 is already a complete stream response and is returned as the
/// error arm.
fn plan_download(
    headers: &[(String, String)],
    target: &DownloadTarget,
    modified: SystemTime,
    total: u64,
    content_type: &str,
    mut response_headers: Vec<(String, String)>,
) -> Result<DownloadPlan, DownloadStream> {
    let (status, start, length, extra) =
        match decide_download(headers, target.id(), modified, total) {
            DownloadDecision::NotModified => {
                return Err(DownloadStream::response(http::response_with_headers(
                    304,
                    content_type,
                    &response_headers,
                    b"",
                )))
            }
            DownloadDecision::PreconditionFailed => {
                return Err(conditional_error(
                    target,
                    412,
                    "precondition_failed",
                    "precondition failed",
                ))
            }
            DownloadDecision::Unsatisfiable => {
                return Err(range_error(target, &response_headers, total))
            }
            DownloadDecision::Full => (200, 0, total, Vec::new()),
            DownloadDecision::Range(start, end) => (
                206,
                start,
                end - start + 1,
                vec![(
                    "Content-Range".to_string(),
                    format!("bytes {}-{}/{}", start, end, total),
                )],
            ),
        };
    response_headers.extend(extra);
    Ok(DownloadPlan {
        head: http::response_head(status, content_type, &response_headers, length),
        start,
        length,
    })
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
    headers: &[(String, String)],
    id: &str,
    modified: SystemTime,
    total: u64,
) -> DownloadDecision {
    if let Some(value) = http::header(headers, "if-match") {
        if !etag_matches(value, id, true) {
            return DownloadDecision::PreconditionFailed;
        }
    } else if let Some(date) =
        http::header(headers, "if-unmodified-since").and_then(http::parse_http_date)
    {
        if modified > date + Duration::from_secs(1) {
            return DownloadDecision::PreconditionFailed;
        }
    }
    if let Some(value) = http::header(headers, "if-none-match") {
        if etag_matches(value, id, false) {
            return DownloadDecision::NotModified;
        }
    } else if let Some(date) =
        http::header(headers, "if-modified-since").and_then(http::parse_http_date)
    {
        if modified <= date + Duration::from_secs(1) {
            return DownloadDecision::NotModified;
        }
    }
    if let Some(value) = http::header(headers, "range") {
        let allow = match http::header(headers, "if-range") {
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

fn range_error(
    target: &DownloadTarget,
    base_headers: &[(String, String)],
    total: u64,
) -> DownloadStream {
    let body = serde_json::to_vec(&serde_json::json!({
        "error": {
            "code": "range_not_satisfiable",
            "message": "range is not satisfiable",
        }
    }))
    .unwrap_or_default();
    let mut headers = base_headers.to_vec();
    headers.push(("Content-Range".to_string(), format!("bytes */{}", total)));
    if target.head_only() {
        DownloadStream::response(http::response_head(
            416,
            "application/json",
            &headers,
            body.len() as u64,
        ))
    } else {
        DownloadStream::response(http::response_with_headers(
            416,
            "application/json",
            &headers,
            &body,
        ))
    }
}

fn not_found(target: &DownloadTarget) -> DownloadStream {
    conditional_error(target, 404, "not_found", "blob not found")
}

/// HEAD responses must not carry a body even on errors; keep the equivalent
/// `Content-Length` so the response still describes the representation.
fn conditional_error(
    target: &DownloadTarget,
    status: u16,
    code: &str,
    message: &str,
) -> DownloadStream {
    if target.head_only() {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {"code": code, "message": message}
        }))
        .unwrap_or_default();
        DownloadStream::response(http::response_head(
            status,
            "application/json",
            &[],
            body.len() as u64,
        ))
    } else {
        DownloadStream::response(http::json_error(status, code, message))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lan::AuthKind;
    use std::io::{Read, Seek, SeekFrom};

    fn request(method: &str, path: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
            peer: "127.0.0.1".parse().expect("peer"),
            principal: AuthKind::None,
            body: Vec::new(),
        }
    }

    fn open(method: &str, path: &str, headers: &[(&str, &str)]) -> DownloadStream {
        let request = request(method, path, headers);
        let target = download_target(method, path).expect("download route");
        DownloadStream::open(&request, target)
    }

    /// Drive the handler the way the Unix driver does: one bounded output
    /// buffer, file sends performed, then a fresh Writable event.
    fn drive(mut handler: DownloadStream, limit: usize) -> (Vec<u8>, CloseReason) {
        let mut all = Vec::new();
        let mut out = Vec::new();
        for _ in 0..10_000 {
            let step =
                handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
            assert!(
                out.len() <= limit,
                "a writable event must respect the output buffer limit"
            );
            all.extend_from_slice(&out);
            out.clear();
            match step.action {
                StreamAction::Close(reason) => return (all, reason),
                StreamAction::SendFile(mut send) => {
                    send.file
                        .seek(SeekFrom::Start(send.offset))
                        .expect("seek file send");
                    let mut reader = (&mut send.file).take(send.length);
                    reader.read_to_end(&mut all).expect("read file send");
                }
                StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {}
                StreamAction::SubmitBlocking(_) => panic!("a download submits no blocking job"),
            }
        }
        panic!("the handler never finished");
    }

    fn body(bytes: &[u8]) -> &[u8] {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response head terminator");
        &bytes[split + 4..]
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Use the content-store root the blob unit tests also use, so the two
    /// test modules never point `SVCD_TEMP_DIR` at different trees.
    fn test_temp() {
        let _ = crate::blob::test_temp_dir();
    }

    fn blob_of(bytes: &[u8]) -> String {
        test_temp();
        crate::blob::put(bytes).expect("write test blob").id
    }

    fn file_of(parts: &[&[u8]], name: Option<&str>, mime: Option<&str>) -> String {
        test_temp();
        let upload = crate::blob::create_upload(crate::blob::UploadKind::File).expect("upload");
        for (index, content) in parts.iter().enumerate() {
            let mut writer = crate::blob::PartWriter::begin(
                &upload,
                index as u32 + 1,
                crate::blob::UploadKind::File,
            )
            .expect("part");
            writer.write(content).expect("write part");
            writer.finish().expect("finish part");
        }
        crate::blob::complete_file(
            &upload,
            None,
            name.map(str::to_string),
            mime.map(str::to_string),
        )
        .expect("complete file")
        .id
    }

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
    fn only_get_and_head_downloads_are_claimed() {
        assert!(matches!(
            download_target("GET", "/api/v1/blobs/sha256:abc").expect("blob"),
            DownloadTarget::Blob { .. }
        ));
        assert!(matches!(
            download_target("HEAD", "/api/v1/file/sha256:abc?x=1").expect("file"),
            DownloadTarget::File {
                head_only: true,
                ..
            }
        ));
        assert!(matches!(
            download_target("GET", "/api/v1/files/sha256:abc").expect("alias"),
            DownloadTarget::File {
                head_only: false,
                ..
            }
        ));
        assert!(download_target("POST", "/api/v1/blobs/sha256:abc").is_none());
        assert!(download_target("GET", "/api/v1/blobs").is_none());
        assert!(download_target("GET", "/api/v1/blobs/uploads/sha256:abc").is_none());
        assert!(download_target("GET", "/api/v1/file/uploads/sha256:abc").is_none());
    }

    #[test]
    fn a_full_download_writes_the_same_head_and_body() {
        let id = blob_of(b"0123456789");
        let path = format!("/api/v1/blobs/{}", id);
        let (bytes, reason) = drive(open("GET", &path, &[]), 4);
        assert_eq!(reason, CloseReason::Completed);
        let head = text(&bytes);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: application/octet-stream\r\n"));
        assert!(head.contains("Content-Length: 10\r\n"));
        assert!(head.contains(&format!("ETag: \"{}\"\r\n", id)));
        assert!(head.contains("Accept-Ranges: bytes\r\n"));
        assert!(head.contains("Cache-Control: no-cache\r\n"));
        assert!(head.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(head.contains("Last-Modified: "));
        assert!(head.contains("Connection: close\r\n"));
        assert_eq!(body(&bytes), b"0123456789");
    }

    #[test]
    fn range_and_conditional_requests_keep_their_old_decisions() {
        let id = blob_of(b"0123456789");
        let path = format!("/api/v1/blobs/{}", id);
        let etag = format!("\"{}\"", id);

        let (range, _) = drive(open("GET", &path, &[("Range", "bytes=2-5")]), 4096);
        let range_text = text(&range);
        assert!(range_text.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(range_text.contains("Content-Range: bytes 2-5/10\r\n"));
        assert_eq!(body(&range), b"2345");

        let (suffix, _) = drive(open("GET", &path, &[("Range", "bytes=-3")]), 4096);
        assert!(text(&suffix).starts_with("HTTP/1.1 206 Partial Content"));
        assert_eq!(body(&suffix), b"789");

        let (not_modified, _) = drive(open("GET", &path, &[("If-None-Match", &etag)]), 4096);
        assert!(text(&not_modified).starts_with("HTTP/1.1 304 Not Modified\r\n"));
        assert!(body(&not_modified).is_empty());

        let (failed, _) = drive(
            open("GET", &path, &[("If-Match", "\"sha256:deadbeef\"")]),
            4096,
        );
        assert!(text(&failed).starts_with("HTTP/1.1 412 Precondition Failed\r\n"));

        let (unsatisfiable, _) = drive(open("GET", &path, &[("Range", "bytes=99-")]), 4096);
        let unsatisfiable_text = text(&unsatisfiable);
        assert!(unsatisfiable_text.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"));
        assert!(unsatisfiable_text.contains("Content-Range: bytes */10\r\n"));

        let (head, _) = drive(open("HEAD", &path, &[]), 4096);
        let head_text = text(&head);
        assert!(head_text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head_text.contains("Content-Length: 10\r\n"));
        assert!(body(&head).is_empty());

        let (missing, _) = drive(open("HEAD", "/api/v1/blobs/sha256:deadbeef", &[]), 4096);
        let missing_text = text(&missing);
        assert!(missing_text.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(missing_text.contains("Content-Length: "));
        assert!(body(&missing).is_empty());
    }

    #[test]
    fn a_file_bundle_download_streams_each_part_in_order() {
        // Different content from the blob module's `hello world!` test: the
        // store is content-addressed, so identical bytes would reuse that
        // manifest (and its name) instead of creating one with our filename.
        let id = file_of(
            &[b"hello ", b"world", b"?"],
            Some("greeting.txt"),
            Some("text/plain"),
        );
        let path = format!("/api/v1/file/{}", id);
        let (full, reason) = drive(open("GET", &path, &[]), 8);
        assert_eq!(reason, CloseReason::Completed);
        let full_text = text(&full);
        assert!(full_text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(full_text.contains("Content-Type: text/plain\r\n"));
        assert!(full_text.contains("Content-Length: 12\r\n"));
        assert!(full_text.contains(&format!("ETag: \"{}\"\r\n", id)));
        assert!(
            full_text.contains("Content-Disposition: attachment; filename=\"greeting.txt\"\r\n")
        );
        assert_eq!(body(&full), b"hello world?");

        let (range, _) = drive(open("GET", &path, &[("Range", "bytes=4-8")]), 4096);
        let range_text = text(&range);
        assert!(range_text.starts_with("HTTP/1.1 206 Partial Content\r\n"));
        assert!(range_text.contains("Content-Range: bytes 4-8/12\r\n"));
        assert_eq!(body(&range), b"o wor");

        let (head, _) = drive(open("HEAD", &path, &[]), 4096);
        let head_text = text(&head);
        assert!(head_text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head_text.contains("Content-Length: 12\r\n"));
        assert!(body(&head).is_empty());
    }

    #[test]
    fn a_large_download_hands_the_body_to_the_server_instead_of_buffering_it() {
        let id = blob_of(&vec![0x5a; 1024 * 1024]);
        let path = format!("/api/v1/blobs/{}", id);
        let mut handler = open("GET", &path, &[]);
        let mut out = Vec::new();
        let mut sends = 0;
        let mut head_bytes = 0;
        for _ in 0..100 {
            let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 1024));
            assert!(out.len() <= 1024, "the output buffer stays bounded");
            head_bytes += out.len();
            out.clear();
            match step.action {
                StreamAction::SendFile(send) => {
                    assert_eq!(send.length, 1024 * 1024);
                    sends += 1;
                }
                StreamAction::Close(_) => break,
                _ => {}
            }
        }
        assert_eq!(sends, 1, "the whole body is one server-side file send");
        assert!(
            head_bytes < 4096,
            "only the head is buffered, not the 1 MiB body: {head_bytes}"
        );
    }

    #[test]
    fn a_missing_blob_is_a_streamed_404() {
        let (bytes, reason) = drive(open("GET", "/api/v1/blobs/sha256:deadbeef", &[]), 4096);
        assert_eq!(reason, CloseReason::Completed);
        let response = text(&bytes);
        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
        let value: serde_json::Value = serde_json::from_slice(body(&bytes)).expect("json error");
        assert_eq!(value["error"]["code"], "not_found");
    }

    #[test]
    fn the_download_handler_is_a_long_lived_stream() {
        let handler = DownloadStream::response(Vec::new());
        assert_eq!(handler.stream_class(), StreamClass::LongLived);
    }
}
