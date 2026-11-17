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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::blob;
use crate::http;
use crate::server::http::{
    BlockingOutcome, CloseReason, FileSend, OutBuffer, Request, StreamAction, StreamClass,
    StreamEvent, StreamHandler, StreamStep,
};

/// One recognized download. Uploads are claimed by `face::blob_upload` before
/// the request body is read.
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
///
/// The response head and the file handle are produced by one blocking job
/// (#132): `locate`, `metadata`, `touch`, `File::open` and the manifest
/// read must not run on the server loop, or a slow `temp_dir` (UNC/SMB/slow
/// disk/AV scan) freezes every HTTP connection, the control channel and the
/// MCP SSE streams. `BlockingOutcome` cannot carry a `File`, so the job
/// stores the opened value in a shared slot; the handler takes it when the
/// server delivers `BlockingDone` (the upload path uses the same shape).
pub struct DownloadStream {
    target: DownloadTarget,
    /// Request headers for the open job; taken by the first Writable.
    headers: Vec<(String, String)>,
    /// The opened value written by the pool job and taken by the handler.
    opened: Arc<Mutex<Option<Opened>>>,
    phase: Phase,
    /// Complete response head. Content-Length describes the representation,
    /// so a HEAD response carries it without a body.
    head: Vec<u8>,
    head_sent: usize,
    body: Body,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No job submitted yet; the first Writable does the disk work on the pool.
    Pending,
    /// The open job is queued or running.
    Blocking,
    /// The job stored a head and body; write them in order.
    Ready,
    /// The connection is gone; a late completion must not write.
    Done,
}

/// The result of the one open job. 304/404/412/416 are rendered in the job as
/// complete head bytes; a refused submit renders the 503 busy envelope in the
/// handler. Either way the handler writes the complete head before it closes.
struct Opened {
    head: Vec<u8>,
    body: Body,
}

impl Opened {
    /// A response whose body is already inside `head` (errors, 304, HEAD).
    fn response(head: Vec<u8>) -> Opened {
        Opened {
            head,
            body: Body::Done,
        }
    }
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
    /// Resolve one recognized download. This only captures the request values:
    /// every filesystem call happens in the blocking job submitted by the
    /// first Writable, so opening the handler cannot stall the server loop.
    pub fn open(request: &Request, target: DownloadTarget) -> DownloadStream {
        DownloadStream {
            target,
            headers: request.headers.clone(),
            opened: Arc::new(Mutex::new(None)),
            phase: Phase::Pending,
            head: Vec::new(),
            head_sent: 0,
            body: Body::Done,
        }
    }

    /// Submit the one disk job. Headers and target move into the job; the
    /// handler keeps the shared slot and the response state.
    fn submit_open(&mut self) -> StreamStep {
        let headers = std::mem::take(&mut self.headers);
        let target = self.target.clone();
        let slot = self.opened.clone();
        self.phase = Phase::Blocking;
        StreamStep::wait(StreamAction::SubmitBlocking(Box::new(move |_budget| {
            let opened = open_download(&headers, &target);
            *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(opened);
            BlockingOutcome::Ok(Vec::new())
        })))
    }

    fn install(&mut self, opened: Opened) {
        self.head = opened.head;
        self.head_sent = 0;
        self.body = opened.body;
        self.phase = Phase::Ready;
    }

    fn on_blocking_done(
        &mut self,
        outcome: BlockingOutcome,
        out: &mut OutBuffer<'_>,
    ) -> StreamStep {
        if self.phase != Phase::Blocking {
            // A completion after Closed: drain the slot so a file the job
            // opened is dropped now, not at handler teardown. No bytes may
            // reach a connection that is already gone.
            let _ = self
                .opened
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            return StreamStep::wait(StreamAction::Continue);
        }
        match outcome {
            BlockingOutcome::Ok(_) => {
                let opened = self
                    .opened
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                debug_assert!(
                    opened.is_some(),
                    "the download open job always stores its result"
                );
                let Some(opened) = opened else {
                    // Unreachable: the job stores the Opened before it returns
                    // Ok. Never write a half response; log and close instead.
                    crate::log::svcd(
                        "svcd: download open job returned no result; closing the connection",
                    );
                    self.phase = Phase::Done;
                    return StreamStep::wait(StreamAction::Close(CloseReason::Failed(
                        "download open job returned no result".into(),
                    )));
                };
                self.install(opened);
            }
            BlockingOutcome::Failed(_) => {
                // The pool only refuses a submit (full queue, the connection
                // already owns a job, or shutdown); every one is a retryable
                // busy condition and no header has been sent yet. This is the
                // one new failure mode of the download path (#132, ADR-0016).
                self.install(Opened::response(busy_response(&self.target)));
            }
        }
        self.on_writable(out)
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        match self.phase {
            Phase::Pending => return self.submit_open(),
            Phase::Blocking => return StreamStep::wait(StreamAction::NeedWrite),
            Phase::Done => return StreamStep::wait(StreamAction::Continue),
            Phase::Ready => {}
        }
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
                // Per-part opens stay here on purpose: the head is already on
                // the wire, so this path cannot answer 503 and must not take a
                // pool round trip (#132 keeps the shard open as it was).
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
                self.phase = Phase::Done;
                self.body = Body::Done;
                self.head.clear();
                self.head_sent = 0;
                // Drop a file the job already opened. A job still running
                // holds its own slot clone; if it finishes after this point,
                // on_blocking_done drains the slot, and if the handler is gone
                // the job's clone drop releases it.
                let _ = self
                    .opened
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                StreamStep::wait(StreamAction::Close(reason))
            }
            StreamEvent::BlockingDone(outcome) => self.on_blocking_done(outcome, out),
            // A download has no request body.
            StreamEvent::Data(_) => StreamStep::wait(StreamAction::Continue),
        }
    }

    fn stream_class(&self) -> StreamClass {
        StreamClass::LongLived
    }
}

/// Run the one open job off the server loop. Every failure that the old
/// synchronous path could answer before the head is rendered here, in the same
/// order, so the wire bytes do not change.
fn open_download(headers: &[(String, String)], target: &DownloadTarget) -> Opened {
    match target {
        DownloadTarget::Blob { .. } => blob_download(headers, target),
        DownloadTarget::File { .. } => file_download(headers, target),
    }
}

fn blob_download(headers: &[(String, String)], target: &DownloadTarget) -> Opened {
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
        Err(head) => return Opened::response(head),
    };
    if target.head_only() {
        return Opened::response(plan.head);
    }
    // Open before the server emits headers: if cleanup raced the metadata
    // snapshot, a 404 is still possible instead of a truncated 200.
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(_) => return not_found(target),
    };
    Opened {
        head: plan.head,
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

fn file_download(headers: &[(String, String)], target: &DownloadTarget) -> Opened {
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
        Err(head) => {
            return Opened {
                head,
                body: Body::Done,
            }
        }
    };
    if target.head_only() || plan.length == 0 {
        return Opened {
            head: plan.head,
            body: Body::Done,
        };
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
    Opened {
        head: plan.head,
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
/// A 304/412/416 is already a complete response and is returned as the error
/// arm; rendering it here keeps the decision inside the open job.
fn plan_download(
    headers: &[(String, String)],
    target: &DownloadTarget,
    modified: SystemTime,
    total: u64,
    content_type: &str,
    mut response_headers: Vec<(String, String)>,
) -> Result<DownloadPlan, Vec<u8>> {
    let (status, start, length, extra) =
        match decide_download(headers, target.id(), modified, total) {
            DownloadDecision::NotModified => {
                return Err(http::response_with_headers(
                    304,
                    content_type,
                    &response_headers,
                    b"",
                ))
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

fn range_error(target: &DownloadTarget, base_headers: &[(String, String)], total: u64) -> Vec<u8> {
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
        http::response_head(416, "application/json", &headers, body.len() as u64)
    } else {
        http::response_with_headers(416, "application/json", &headers, &body)
    }
}

fn not_found(target: &DownloadTarget) -> Opened {
    Opened::response(conditional_error(
        target,
        404,
        "not_found",
        "blob not found",
    ))
}

/// The pool refused the open job before any header: a GET answers the shared
/// 503 busy envelope, a HEAD keeps its no-body shape and the same
/// `Content-Length` (the server's own 503 also carries `Retry-After`).
fn busy_response(target: &DownloadTarget) -> Vec<u8> {
    let response = crate::server::http::busy_response("blocking queue is full");
    if target.head_only() {
        // Keep GET's Content-Length and Retry-After, drop only the body.
        http::strip_body(response)
    } else {
        response
    }
}

/// HEAD responses must not carry a body even on errors; keep the equivalent
/// `Content-Length` so the response still describes the representation.
fn conditional_error(target: &DownloadTarget, status: u16, code: &str, message: &str) -> Vec<u8> {
    if target.head_only() {
        let body = serde_json::to_vec(&serde_json::json!({
            "error": {"code": code, "message": message}
        }))
        .unwrap_or_default();
        http::response_head(status, "application/json", &[], body.len() as u64)
    } else {
        http::json_error(status, code, message)
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
            deadline: None,
            cancel: None,
        }
    }

    fn open(method: &str, path: &str, headers: &[(&str, &str)]) -> DownloadStream {
        let request = request(method, path, headers);
        let target = download_target(method, path).expect("download route");
        DownloadStream::open(&request, target)
    }

    /// Drive the handler the way the Unix driver does: one bounded output
    /// buffer, the submitted open job run inline, file sends performed, then a
    /// fresh Writable event.
    fn drive(mut handler: DownloadStream, limit: usize) -> (Vec<u8>, CloseReason) {
        let mut all = Vec::new();
        let mut out = Vec::new();
        let mut step =
            handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
        for _ in 0..10_000 {
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
                StreamAction::SubmitBlocking(job) => {
                    let outcome = job(crate::server::http::JobBudget::for_test());
                    step = handler.on_event(
                        StreamEvent::BlockingDone(outcome),
                        &mut OutBuffer::new(&mut out, limit),
                    );
                    continue;
                }
                StreamAction::Continue | StreamAction::NeedRead | StreamAction::NeedWrite => {}
            }
            step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
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
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 1024));
        let StreamAction::SubmitBlocking(job) = step.action else {
            panic!("the first Writable submits the open job");
        };
        let mut step = handler.on_event(
            StreamEvent::BlockingDone(job(crate::server::http::JobBudget::for_test())),
            &mut OutBuffer::new(&mut out, 1024),
        );
        let mut sends = 0;
        let mut head_bytes = 0;
        for _ in 0..100 {
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
            step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 1024));
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

    /// Take the one job submitted by the first Writable and run it the way a
    /// pool worker would. The step before it must write nothing: all locate /
    /// metadata / touch / open / plan work belongs to the job.
    fn run_open_job(handler: &mut DownloadStream) -> BlockingOutcome {
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        assert!(out.is_empty(), "the first Writable must not write bytes");
        let StreamAction::SubmitBlocking(job) = step.action else {
            panic!("the first Writable must submit the open job");
        };
        job(crate::server::http::JobBudget::for_test())
    }

    #[test]
    fn the_first_writable_submits_the_open_job_before_any_bytes() {
        let id = blob_of(b"0123456789");
        let path = format!("/api/v1/blobs/{}", id);
        let mut handler = open("GET", &path, &[]);
        let outcome = run_open_job(&mut handler);
        let mut out = Vec::new();
        let step = handler.on_event(
            StreamEvent::BlockingDone(outcome),
            &mut OutBuffer::new(&mut out, 4096),
        );
        let head = text(&out);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.ends_with("\r\n\r\n"), "only the head is written");
        let StreamAction::SendFile(send) = step.action else {
            panic!("the opened body is handed to the server as a file send");
        };
        assert_eq!(send.length, 10);
        assert!(send.offset == 0);
    }

    #[test]
    fn a_missing_blob_renders_404_through_blocking_done() {
        let mut handler = open("GET", "/api/v1/blobs/sha256:deadbeef", &[]);
        let outcome = run_open_job(&mut handler);
        let mut out = Vec::new();
        let step = handler.on_event(
            StreamEvent::BlockingDone(outcome),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let response = text(&out);
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{response}"
        );
        let value: serde_json::Value = serde_json::from_slice(body(&out)).expect("json error");
        assert_eq!(value["error"]["code"], "not_found");
    }

    #[test]
    fn conditional_and_range_failures_arrive_through_blocking_done() {
        let id = blob_of(b"0123456789");
        let path = format!("/api/v1/blobs/{}", id);
        let etag = format!("\"{}\"", id);
        let cases: [(Vec<(&str, &str)>, u16); 3] = [
            (vec![("If-Match", "\"sha256:deadbeef\"")], 412),
            (vec![("Range", "bytes=99-")], 416),
            (vec![("If-None-Match", etag.as_str())], 304),
        ];
        for (headers, status) in cases {
            let mut handler = open("GET", &path, &headers);
            let outcome = run_open_job(&mut handler);
            let mut out = Vec::new();
            let step = handler.on_event(
                StreamEvent::BlockingDone(outcome),
                &mut OutBuffer::new(&mut out, 4096),
            );
            assert!(
                matches!(step.action, StreamAction::Close(CloseReason::Completed)),
                "{status} closes after its complete head"
            );
            let response = text(&out);
            assert!(
                response.starts_with(&format!("HTTP/1.1 {} ", status)),
                "{status}: {response}"
            );
        }
    }

    #[test]
    fn a_full_queue_turns_the_download_into_503_busy() {
        let id = blob_of(b"0123456789");
        let path = format!("/api/v1/blobs/{}", id);
        let mut handler = open("GET", &path, &[]);
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        let StreamAction::SubmitBlocking(_job) = step.action else {
            panic!("the first Writable submits the open job");
        };
        let step = handler.on_event(
            StreamEvent::BlockingDone(BlockingOutcome::Failed("the blocking queue is full".into())),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let response = text(&out);
        assert!(
            response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{response}"
        );
        assert!(response.contains("Retry-After: 1\r\n"), "{response}");
        assert!(response.contains("\"code\":\"busy\""), "{response}");
    }

    #[test]
    fn a_head_download_refused_by_the_pool_has_no_body() {
        let mut handler = open("HEAD", "/api/v1/blobs/sha256:deadbeef", &[]);
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        assert!(matches!(step.action, StreamAction::SubmitBlocking(_)));
        let step = handler.on_event(
            StreamEvent::BlockingDone(BlockingOutcome::Failed("the blocking queue is full".into())),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::Completed)
        ));
        let response = text(&out);
        assert!(
            response.starts_with("HTTP/1.1 503 Service Unavailable\r\n"),
            "{response}"
        );
        assert!(response.contains("Content-Length: "), "{response}");
        assert!(body(&out).is_empty(), "HEAD must not write the JSON body");
    }

    #[test]
    fn a_late_completion_after_close_is_discarded() {
        let id = blob_of(b"0123456789");
        let path = format!("/api/v1/blobs/{}", id);
        let mut handler = open("GET", &path, &[]);
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        let StreamAction::SubmitBlocking(job) = step.action else {
            panic!("the first Writable submits the open job");
        };
        let closed = handler.on_event(
            StreamEvent::Closed(CloseReason::PeerClosed),
            &mut OutBuffer::new(&mut out, 0),
        );
        assert!(matches!(
            closed.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
        // The worker finished after the connection went away; the file it
        // opened is dropped with the shared slot and no bytes are written.
        let outcome = job(crate::server::http::JobBudget::for_test());
        let late = handler.on_event(
            StreamEvent::BlockingDone(outcome),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(late.action, StreamAction::Continue));
        assert!(out.is_empty(), "a late completion writes nothing");
        assert!(
            handler
                .opened
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_none(),
            "a late result is drained instead of keeping its file in the shared slot"
        );
    }

    #[test]
    fn the_download_handler_is_a_long_lived_stream() {
        let handler = open("GET", "/api/v1/blobs/sha256:deadbeef", &[]);
        assert_eq!(handler.stream_class(), StreamClass::LongLived);
    }
}
