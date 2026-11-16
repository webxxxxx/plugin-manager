//! Blob and file uploads as a sans-IO stream.
//!
//! The server owns the socket and request framing; this handler receives body
//! bytes, stages them through the blocking pool, and answers once. A slow or
//! disconnected client pauses input while one chunk is in flight, so staging
//! never grows an unbounded queue. Simple, chunked, multipart and resumable
//! part uploads all use the same state machine.

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::blob;
use crate::http;
use crate::lan::{AuthKind, Lan};
use crate::server::http::{
    BlockingOutcome, CloseReason, Exchange, OutBuffer, StreamAction, StreamClass, StreamEvent,
    StreamHandler, StreamStep,
};

const MULTIPART_OVERHEAD: u64 = 8 * 1024 * 1024;

/// One recognized upload route. Session metadata (create/status/complete/abort)
/// stays on the ordinary buffered router.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadTarget {
    Blob,
    Part {
        upload: String,
        part: u32,
        kind: blob::UploadKind,
    },
}

/// Recognize the streaming upload routes. The path parser mirrors the old
/// takeover: /api/v1/files/{id} is the historical alias for /api/v1/file/{id}.
pub fn upload_target(head: &http::Head) -> Option<UploadTarget> {
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
        ("POST", ["api", "v1", "blobs"]) => Some(UploadTarget::Blob),
        ("PUT", ["api", "v1", "blobs", "uploads", upload, "parts", part]) => {
            Some(UploadTarget::Part {
                upload: http::percent_decode(upload, false),
                part: part.parse::<u32>().unwrap_or(0),
                kind: blob::UploadKind::Blob,
            })
        }
        ("PUT", ["api", "v1", "file", "uploads", upload, "parts", part]) => {
            Some(UploadTarget::Part {
                upload: http::percent_decode(upload, false),
                part: part.parse::<u32>().unwrap_or(0),
                kind: blob::UploadKind::File,
            })
        }
        _ => None,
    }
}

/// Resolve auth and endpoint policy, validate the framing, and return the
/// stream handler. Errors are complete response bytes: the server writes them
/// and closes without reading a body.
pub fn exchange(head: &http::Head, lan: &Arc<Mutex<Lan>>) -> Result<Exchange, Vec<u8>> {
    let Some(target) = upload_target(head) else {
        return Err(http::json_error(404, "not_found", "not an upload route"));
    };
    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&head.headers) {
            Ok(auth) => auth,
            Err(()) => {
                return Err(http::json_error(
                    401,
                    "unauthorized",
                    "authentication required",
                ))
            }
        },
        Err(_) => return Err(http::json_error(500, "internal", "LAN state unavailable")),
    };
    if auth == AuthKind::Cookie
        && http::is_write_method(&head.method)
        && !http::csrf_ok(&head.headers)
    {
        return Err(http::json_error(
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        ));
    }

    let content_type = http::header(&head.headers, "content-type").unwrap_or("");
    let boundary = match &target {
        UploadTarget::Blob => multipart_boundary(content_type)
            .map_err(|message| http::json_error(400, "bad_request", &message))?,
        UploadTarget::Part { .. } => None,
    };
    let is_multipart = boundary.is_some();
    let multipart = boundary.map(|boundary| MultipartParser::new(&boundary));
    let limit = match &target {
        UploadTarget::Blob if is_multipart => blob::MAX_BLOB_BYTES + MULTIPART_OVERHEAD,
        UploadTarget::Blob => blob::MAX_BLOB_BYTES,
        UploadTarget::Part { .. } => blob::MAX_FILE_PART_BYTES,
    };
    let framing = http::framing(&head.headers)
        .map_err(|message| http::json_error(400, "bad_request", &message))?;
    match framing {
        http::Framing::None | http::Framing::Length(0) => {
            return Err(http::json_error(
                400,
                "bad_request",
                if matches!(target, UploadTarget::Blob) {
                    "empty blob body"
                } else {
                    "empty part body"
                },
            ))
        }
        http::Framing::Length(length) if length > limit => {
            return Err(http::json_error(
                413,
                "payload_too_large",
                "request body is too large",
            ))
        }
        _ => {}
    }
    if let UploadTarget::Part { upload, part, kind } = &target {
        if !blob::valid_upload_id(upload) {
            return Err(http::json_error(400, "bad_request", "invalid upload id"));
        }
        if *part == 0 || *part > blob::MAX_PART_NUMBER {
            return Err(http::json_error(
                400,
                "bad_request",
                "part number must be between 1 and 10000",
            ));
        }
        if let Err(message) = blob::require_upload_kind(upload, *kind) {
            return Err(status_response(&message));
        }
    }

    let handler = BlobUploadStream::new(target, framing, multipart, limit, expects_continue(head));
    Ok(Exchange::Stream(Box::new(handler)))
}

fn expects_continue(head: &http::Head) -> bool {
    http::header(&head.headers, "expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
        && head.version.eq_ignore_ascii_case("HTTP/1.1")
}

/// One upload body as a sans-IO state machine. Response bytes are small JSON;
// body bytes are owned by blocking jobs and never accumulate in the output
// buffer.
pub struct BlobUploadStream {
    state: Arc<Mutex<UploadState>>,
    phase: Phase,
    expect_continue: bool,
    continue_sent: bool,
    response: Vec<u8>,
    response_sent: usize,
}

enum Phase {
    Body,
    Blocking,
    Responding,
    Done,
}

struct UploadState {
    target: UploadTarget,
    framing: BodyFraming,
    multipart: Option<MultipartParser>,
    upload: Option<blob::Upload>,
    part: Option<blob::PartWriter>,
    finished: Option<Result<Success, String>>,
}

enum BodyFraming {
    Length { remaining: u64 },
    Chunked(http::ChunkedDecoder),
}

enum Success {
    Blob(blob::Meta),
    Part(blob::PartInfo),
}

impl BlobUploadStream {
    fn new(
        target: UploadTarget,
        framing: http::Framing,
        multipart: Option<MultipartParser>,
        limit: u64,
        expect_continue: bool,
    ) -> BlobUploadStream {
        let framing = match framing {
            http::Framing::Length(remaining) => BodyFraming::Length { remaining },
            http::Framing::Chunked => BodyFraming::Chunked(http::ChunkedDecoder::new(limit)),
            http::Framing::None => BodyFraming::Length { remaining: 0 },
        };
        BlobUploadStream {
            state: Arc::new(Mutex::new(UploadState {
                target,
                framing,
                multipart,
                upload: None,
                part: None,
                finished: None,
            })),
            phase: Phase::Body,
            expect_continue,
            continue_sent: false,
            response: Vec::new(),
            response_sent: 0,
        }
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        if !self.continue_sent && self.expect_continue {
            if out.write(b"HTTP/1.1 100 Continue\r\n\r\n") {
                self.continue_sent = true;
                return StreamStep::wait(StreamAction::NeedRead);
            }
            return StreamStep::wait(StreamAction::NeedWrite);
        }
        if self.response_sent < self.response.len() {
            let take = (self.response.len() - self.response_sent).min(out.remaining());
            out.write(&self.response[self.response_sent..self.response_sent + take]);
            self.response_sent += take;
            if self.response_sent == self.response.len() {
                return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
            }
            return StreamStep::wait(StreamAction::NeedWrite);
        }
        if !self.response.is_empty() {
            return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
        }
        StreamStep::wait(StreamAction::NeedRead)
    }

    fn on_data(&mut self, data: &[u8]) -> StreamStep {
        if !matches!(self.phase, Phase::Body) || data.is_empty() {
            return StreamStep::wait(StreamAction::NeedRead);
        }
        let chunk = data.to_vec();
        let state = self.state.clone();
        self.phase = Phase::Blocking;
        StreamStep::wait(StreamAction::SubmitBlocking(Box::new(move || {
            run_chunk(&state, chunk)
        })))
    }

    fn on_blocking_done(
        &mut self,
        outcome: BlockingOutcome,
        out: &mut OutBuffer<'_>,
    ) -> StreamStep {
        match outcome {
            BlockingOutcome::Failed(message) => {
                self.respond(status_response(&message));
                self.on_writable(out)
            }
            BlockingOutcome::Ok => {
                let finished = self
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .finished
                    .take();
                match finished {
                    Some(Ok(success)) => {
                        self.respond(success_response(success));
                        self.on_writable(out)
                    }
                    Some(Err(message)) => {
                        self.respond(status_response(&message));
                        self.on_writable(out)
                    }
                    None => {
                        self.phase = Phase::Body;
                        StreamStep::wait(StreamAction::NeedRead)
                    }
                }
            }
        }
    }

    fn respond(&mut self, response: Vec<u8>) {
        self.response = response;
        self.response_sent = 0;
        self.phase = Phase::Responding;
    }
}

impl StreamHandler for BlobUploadStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Data(data) => self.on_data(data),
            StreamEvent::BlockingDone(outcome) => self.on_blocking_done(outcome, out),
            StreamEvent::Tick(_) => StreamStep::wait(StreamAction::Close(CloseReason::TimedOut)),
            StreamEvent::Closed(reason) => {
                self.phase = Phase::Done;
                self.response.clear();
                self.response_sent = 0;
                StreamStep::wait(StreamAction::Close(reason))
            }
        }
    }

    fn stream_class(&self) -> StreamClass {
        StreamClass::LongLived
    }
}

fn run_chunk(state: &Arc<Mutex<UploadState>>, chunk: Vec<u8>) -> BlockingOutcome {
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    match state.feed(&chunk) {
        Ok(()) => BlockingOutcome::Ok,
        Err(message) => BlockingOutcome::Failed(message),
    }
}

impl UploadState {
    fn feed(&mut self, raw: &[u8]) -> Result<(), String> {
        if self.finished.is_some() {
            return Ok(());
        }
        let (body, done) = match &mut self.framing {
            BodyFraming::Length { remaining } => {
                let take = std::cmp::min(raw.len() as u64, *remaining) as usize;
                *remaining -= take as u64;
                (raw[..take].to_vec(), *remaining == 0)
            }
            BodyFraming::Chunked(decoder) => {
                let mut decoded = Vec::new();
                decoder.feed(raw, &mut |chunk| {
                    decoded.extend_from_slice(chunk);
                    Ok(())
                })?;
                (decoded, decoder.is_done())
            }
        };
        if !body.is_empty() {
            self.write_body(&body)?;
        }
        if done {
            self.finish_body()?;
        }
        Ok(())
    }

    fn write_body(&mut self, bytes: &[u8]) -> Result<(), String> {
        match &self.target {
            UploadTarget::Blob => {
                if self.upload.is_none() {
                    self.upload = Some(blob::Upload::begin()?);
                }
                let upload = self.upload.as_mut().expect("blob upload initialized");
                match &mut self.multipart {
                    Some(parser) => parser.push(bytes, upload),
                    None => upload.write(bytes),
                }
            }
            UploadTarget::Part { upload, part, kind } => {
                let upload_id = upload.clone();
                let part = *part;
                let kind = *kind;
                if self.part.is_none() {
                    self.part = Some(blob::PartWriter::begin(&upload_id, part, kind)?);
                }
                self.part
                    .as_mut()
                    .expect("part writer initialized")
                    .write(bytes)
            }
        }
    }

    fn finish_body(&mut self) -> Result<(), String> {
        match &self.target {
            UploadTarget::Blob => {
                if let Some(parser) = &self.multipart {
                    parser.finish()?;
                }
                let upload = self.upload.take().ok_or("empty blob body")?;
                if upload.size() == 0 {
                    return Err("empty blob body".into());
                }
                let meta = upload.finish()?;
                self.finished = Some(Ok(Success::Blob(meta)));
            }
            UploadTarget::Part { .. } => {
                let writer = self.part.take().ok_or("empty part body")?;
                if writer.size() == 0 {
                    return Err("empty part body".into());
                }
                let info = writer.finish()?;
                self.finished = Some(Ok(Success::Part(info)));
            }
        }
        Ok(())
    }
}

fn success_response(success: Success) -> Vec<u8> {
    match success {
        Success::Blob(meta) => {
            let body =
                serde_json::to_vec(&json!({"id": meta.id, "size": meta.size})).unwrap_or_default();
            http::response_with_headers(
                200,
                "application/json",
                &[
                    ("ETag".to_string(), format!("\"{}\"", meta.id)),
                    ("Location".to_string(), format!("/api/v1/blobs/{}", meta.id)),
                    ("Cache-Control".to_string(), "no-store".to_string()),
                ],
                &body,
            )
        }
        Success::Part(info) => {
            let body = serde_json::to_vec(&json!({
                "partNumber": info.number,
                "size": info.size,
                "etag": info.etag,
            }))
            .unwrap_or_default();
            http::response_with_headers(
                200,
                "application/json",
                &[("ETag".to_string(), format!("\"{}\"", info.etag))],
                &body,
            )
        }
    }
}

fn status_response(message: &str) -> Vec<u8> {
    let status = if message.contains("too large") {
        413
    } else if message.contains("unknown upload") {
        404
    } else if message.contains("kind mismatch") || message.contains("kind") {
        409
    } else if message.contains("part number")
        || message.contains("invalid")
        || message.contains("truncated")
        || message.contains("multipart")
        || message.contains("chunk")
        || message.contains("empty")
    {
        400
    } else {
        500
    };
    http::json_error(status, code_for(status), message)
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

/// Extract the first file part from a streaming multipart/form-data body.
/// Parts that have no filename= are skipped; later parts are drained but not
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

fn keep_boundary_tail(data: &[u8], delimiter: &[u8]) -> usize {
    let max = std::cmp::min(data.len(), delimiter.len().saturating_sub(1));
    for length in (1..=max).rev() {
        if data[data.len() - length..] == delimiter[..length] {
            return length;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn parse_head(request: &[u8]) -> http::Head {
        http::parse_head(request).expect("request head").0
    }

    fn framing(head: &http::Head) -> http::Framing {
        http::framing(&head.headers).expect("framing")
    }

    fn handler(head: &http::Head) -> BlobUploadStream {
        let target = upload_target(head).expect("upload route");
        let multipart = match &target {
            UploadTarget::Blob => {
                let content_type = http::header(&head.headers, "content-type").unwrap_or("");
                multipart_boundary(content_type)
                    .expect("boundary")
                    .map(|boundary| MultipartParser::new(&boundary))
            }
            UploadTarget::Part { .. } => None,
        };
        let limit = match &target {
            UploadTarget::Blob if multipart.is_some() => blob::MAX_BLOB_BYTES + MULTIPART_OVERHEAD,
            UploadTarget::Blob => blob::MAX_BLOB_BYTES,
            UploadTarget::Part { .. } => blob::MAX_FILE_PART_BYTES,
        };
        BlobUploadStream::new(
            target,
            framing(head),
            multipart,
            limit,
            expects_continue(head),
        )
    }

    /// Drive the handler the way the server does: write, read body chunks,
    /// run submitted jobs inline, then write the final response.
    fn drive(mut handler: BlobUploadStream, body: &[u8], chunk: usize) -> (Vec<u8>, CloseReason) {
        let _ = crate::blob::test_temp_dir();
        let mut out = Vec::new();
        let mut all = Vec::new();
        let mut offset = 0;
        let limit = 64;
        let mut step =
            handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
        loop {
            all.extend_from_slice(&out);
            out.clear();
            match step.action {
                StreamAction::Close(reason) => return (all, reason),
                StreamAction::SubmitBlocking(job) => {
                    let outcome = job();
                    step = handler.on_event(
                        StreamEvent::BlockingDone(outcome),
                        &mut OutBuffer::new(&mut out, limit),
                    );
                }
                StreamAction::NeedWrite => {
                    step = handler
                        .on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
                }
                StreamAction::NeedRead | StreamAction::Continue => {
                    if offset >= body.len() {
                        panic!("handler still wants body after all bytes");
                    }
                    let end = (offset + chunk).min(body.len());
                    step = handler.on_event(
                        StreamEvent::Data(&body[offset..end]),
                        &mut OutBuffer::new(&mut out, limit),
                    );
                    offset = end;
                }
                StreamAction::SendFile(_) => panic!("an upload sends no file"),
            }
        }
    }

    fn final_body(bytes: &[u8]) -> &[u8] {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response head");
        &bytes[split + 4..]
    }

    fn blob_id(response: &[u8]) -> String {
        let value: serde_json::Value =
            serde_json::from_slice(final_body(response)).expect("json response");
        value["id"].as_str().expect("blob id").to_string()
    }

    #[test]
    fn a_simple_length_upload_stages_and_returns_the_blob() {
        let head = parse_head(
            b"POST /api/v1/blobs HTTP/1.1\r\nHost: x\r\nContent-Length: 9\r\nContent-Type: application/octet-stream\r\n\r\n",
        );
        let (response, reason) = drive(handler(&head), b"blob body", 2);
        assert_eq!(reason, CloseReason::Completed);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("ETag: \"sha256:"), "{text}");
        let id = blob_id(&response);
        let mut content = Vec::new();
        crate::blob::path(&id)
            .and_then(|path| std::fs::File::open(path).map_err(|error| error.to_string()))
            .and_then(|mut file| {
                file.read_to_end(&mut content)
                    .map_err(|error| error.to_string())
            })
            .expect("read staged blob");
        assert_eq!(content, b"blob body");
    }

    #[test]
    fn a_chunked_upload_decodes_the_body() {
        let head = parse_head(
            b"POST /api/v1/blobs HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nContent-Type: application/octet-stream\r\n\r\n",
        );
        let body = b"3\r\nfoo\r\n4\r\n bar\r\n0\r\n\r\n";
        let (response, reason) = drive(handler(&head), body, 1);
        assert_eq!(reason, CloseReason::Completed);
        let id = blob_id(&response);
        let mut content = Vec::new();
        let path = crate::blob::path(&id).expect("blob path");
        std::fs::File::open(path)
            .and_then(|mut file| file.read_to_end(&mut content))
            .expect("read staged blob");
        assert_eq!(content, b"foo bar");
    }

    #[test]
    fn a_multipart_upload_takes_only_the_file_part() {
        let boundary = "svcd-test-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nfile bytes\r\n--{boundary}--\r\n"
        );
        let head = parse_head(
            format!(
                "POST /api/v1/blobs HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nContent-Type: multipart/form-data; boundary={}\r\n\r\n",
                body.len(),
                boundary
            )
            .as_bytes(),
        );
        let (response, reason) = drive(handler(&head), body.as_bytes(), 3);
        assert_eq!(reason, CloseReason::Completed);
        let id = blob_id(&response);
        let mut content = Vec::new();
        let path = crate::blob::path(&id).expect("blob path");
        std::fs::File::open(path)
            .and_then(|mut file| file.read_to_end(&mut content))
            .expect("read staged blob");
        assert_eq!(content, b"file bytes");
    }

    #[test]
    fn a_resumable_part_can_be_reuploaded() {
        let _ = crate::blob::test_temp_dir();
        let upload = crate::blob::create_upload(blob::UploadKind::Blob).expect("upload");
        let path = format!("/api/v1/blobs/uploads/{}/parts/1", upload);
        let head = parse_head(
            format!(
                "PUT {} HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\n",
                path
            )
            .as_bytes(),
        );
        let (first, reason) = drive(handler(&head), b"first", 2);
        assert_eq!(reason, CloseReason::Completed);
        let first_text = String::from_utf8_lossy(&first);
        assert!(first_text.starts_with("HTTP/1.1 200 OK"), "{first_text}");
        assert!(first_text.contains("\"partNumber\":1"));

        let head = parse_head(
            format!(
                "PUT {} HTTP/1.1\r\nHost: x\r\nContent-Length: 6\r\n\r\n",
                path
            )
            .as_bytes(),
        );
        let (second, reason) = drive(handler(&head), b"second", 2);
        assert_eq!(reason, CloseReason::Completed);
        assert!(String::from_utf8_lossy(&second).starts_with("HTTP/1.1 200 OK"));

        let parts = crate::blob::upload_parts(&upload, blob::UploadKind::Blob).expect("parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].size, 6);
        let meta = crate::blob::complete_upload(&upload, None).expect("complete");
        let path = crate::blob::path(&meta.id).expect("path");
        let content = std::fs::read(path).expect("read");
        assert_eq!(content, b"second");
    }

    #[test]
    fn expect_continue_is_sent_before_the_body() {
        let head = parse_head(
            b"POST /api/v1/blobs HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n",
        );
        let (response, reason) = drive(handler(&head), b"body", 2);
        assert_eq!(reason, CloseReason::Completed);
        let text = String::from_utf8_lossy(&response);
        assert!(
            text.starts_with("HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n"),
            "{text}"
        );
    }

    #[test]
    fn only_upload_routes_are_claimed() {
        for (method, path) in [
            ("POST", "/api/v1/blobs"),
            (
                "PUT",
                "/api/v1/blobs/uploads/0123456789abcdef0123456789abcdef/parts/1",
            ),
            (
                "PUT",
                "/api/v1/file/uploads/0123456789abcdef0123456789abcdef/parts/2",
            ),
            (
                "PUT",
                "/api/v1/files/uploads/0123456789abcdef0123456789abcdef/parts/3",
            ),
        ] {
            assert!(
                upload_target(&parse_head(
                    format!("{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\n\r\n")
                        .as_bytes()
                ))
                .is_some(),
                "{method} {path} is an upload"
            );
        }
        for (method, path) in [
            ("GET", "/api/v1/blobs/0123456789abcdef0123456789abcdef"),
            ("GET", "/api/v1/blobs"),
            (
                "POST",
                "/api/v1/file/uploads/0123456789abcdef0123456789abcdef/parts/1",
            ),
            (
                "GET",
                "/api/v1/file/uploads/0123456789abcdef0123456789abcdef/parts/1",
            ),
        ] {
            assert!(
                upload_target(&parse_head(
                    format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()
                ))
                .is_none(),
                "{method} {path} must not use the upload handler"
            );
        }
    }
}
