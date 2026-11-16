//! `/api/v1/export/{id}` (`.spkb`) and `.../{id}/spk` as a sans-IO stream.
//!
//! The response body is the same tar the legacy takeover produced: the same
//! entry order, tar headers, padding and trailer. What changed is who produces
//! it. The handler builds a plan of small byte segments and server-side file
//! sends; the server writes the bytes through the bounded output buffer and the
//! kernel file-send path. No archive is buffered whole, no temporary file is
//! written, and no thread is created per export.
//!
//! One deliberate difference from the legacy writer: it re-hashed every payload
//! file while copying it and aborted on a mismatch. The bytes now go from disk
//! to the socket inside the kernel, so the handler never sees them. The plan
//! still rejects a missing or truncated payload before the headers go out, and
//! the install path verifies `files[].sha256` again on import (#120).

use std::collections::VecDeque;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::http;
use crate::package;
use crate::plugin;
use crate::server::http::{
    CloseReason, FileSend, OutBuffer, StreamAction, StreamClass, StreamEvent, StreamHandler,
    StreamStep,
};

const TRAILER: usize = 1024;

/// One recognized export. The ids are already percent-decoded.
pub enum ExportTarget {
    Spk { id: String },
    Spkb { id: String },
}

impl ExportTarget {
    fn id(&self) -> &str {
        match self {
            ExportTarget::Spk { id } | ExportTarget::Spkb { id } => id,
        }
    }
}

/// Recognize the two export downloads. Method and query handling mirror the
/// legacy route table: only `GET` is exported, and the query string is ignored.
pub fn export_target(method: &str, path: &str) -> Option<ExportTarget> {
    if method != "GET" {
        return None;
    }
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    match parts.as_slice() {
        ["api", "v1", "export", id, "spk"] => Some(ExportTarget::Spk {
            id: http::percent_decode(id, false),
        }),
        ["api", "v1", "export", id] => Some(ExportTarget::Spkb {
            id: http::percent_decode(id, false),
        }),
        _ => None,
    }
}

/// One piece of the response body. Small protocol pieces are bytes; payload
/// files stay on disk and are handed to the server as a file send.
enum Segment {
    Bytes(Vec<u8>),
    File {
        path: PathBuf,
        offset: u64,
        length: u64,
    },
}

/// A complete response: the head, then the body in order.
struct Plan {
    head: Vec<u8>,
    segments: VecDeque<Segment>,
}

/// One export as a sans-IO state machine.
pub struct ExportStream {
    head: Vec<u8>,
    head_sent: usize,
    segments: VecDeque<Segment>,
    /// How much of the current byte segment is already written. Reset when the
    /// segment is finished.
    at: usize,
}

impl ExportStream {
    pub fn open(target: ExportTarget) -> ExportStream {
        let id = target.id();
        // Validate before touching the filesystem: `plugin_dir` joins the id.
        let version = if package::valid_name(id) {
            plugin::current_version(id)
        } else {
            None
        };
        let Some(version) = version else {
            return ExportStream::response(http::json_error(
                404,
                "not_found",
                &format!("unknown plugin: {}", id),
            ));
        };
        ExportStream::from_plan(plan_for(&target, &version))
    }

    fn from_plan(plan: Result<Plan, String>) -> ExportStream {
        match plan {
            Ok(plan) => ExportStream {
                head: plan.head,
                head_sent: 0,
                segments: plan.segments,
                at: 0,
            },
            // The route is streamed, so even a failure is a stream response.
            Err(message) => ExportStream::response(http::json_error(500, "internal", &message)),
        }
    }

    fn response(head: Vec<u8>) -> ExportStream {
        ExportStream {
            head,
            head_sent: 0,
            segments: VecDeque::new(),
            at: 0,
        }
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        loop {
            if self.head_sent < self.head.len() {
                let take = (self.head.len() - self.head_sent).min(out.remaining());
                out.write(&self.head[self.head_sent..self.head_sent + take]);
                self.head_sent += take;
                if self.head_sent < self.head.len() {
                    return StreamStep::wait(StreamAction::NeedWrite);
                }
            }
            let Some(segment) = self.segments.pop_front() else {
                return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
            };
            match segment {
                Segment::Bytes(data) => {
                    let take = (data.len() - self.at).min(out.remaining());
                    out.write(&data[self.at..self.at + take]);
                    self.at += take;
                    if self.at < data.len() {
                        self.segments.push_front(Segment::Bytes(data));
                        return StreamStep::wait(StreamAction::NeedWrite);
                    }
                    self.at = 0;
                }
                Segment::File {
                    path,
                    offset,
                    length,
                } => {
                    let file = match File::open(&path) {
                        Ok(file) => file,
                        Err(error) => {
                            return StreamStep::wait(StreamAction::Close(CloseReason::Failed(
                                format!("open {}: {}", path.display(), error),
                            )))
                        }
                    };
                    return StreamStep::wait(StreamAction::SendFile(FileSend {
                        file,
                        offset,
                        length,
                    }));
                }
            }
        }
    }
}

impl StreamHandler for ExportStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Tick(_) => StreamStep::wait(StreamAction::Close(CloseReason::TimedOut)),
            StreamEvent::Closed(reason) => {
                // A disconnect drops the remaining plan, so no file handle or
                // body segment outlives the connection.
                self.segments.clear();
                self.head.clear();
                self.head_sent = 0;
                StreamStep::wait(StreamAction::Close(reason))
            }
            // An export has no request body and submits no blocking job.
            StreamEvent::Data(_) | StreamEvent::BlockingDone(_) => {
                StreamStep::wait(StreamAction::Continue)
            }
        }
    }

    fn stream_class(&self) -> StreamClass {
        StreamClass::LongLived
    }
}

fn plan_for(target: &ExportTarget, version: &str) -> Result<Plan, String> {
    let id = target.id();
    let version_dir = plugin::plugin_dir(id).join(version);
    match target {
        ExportTarget::Spk { .. } => plan_spk(id, version, &version_dir),
        ExportTarget::Spkb { .. } => {
            let config = match plugin::active_config(id) {
                Some(path) => {
                    Some(fs::read(&path).map_err(|error| format!("read config: {}", error))?)
                }
                None => None,
            };
            plan_spkb(id, version, &version_dir, config.as_deref())
        }
    }
}

/// The installable, signed `.spk` rebuilt from the installed version directory.
fn plan_spk(id: &str, version: &str, version_dir: &Path) -> Result<Plan, String> {
    let (segments, size) = spk_segments(version_dir)?;
    Ok(Plan {
        head: download_head(id, version, "spk", size),
        segments,
    })
}

/// The `.spkb` backup: `backup.json`, the nested `.spk` entry, the active
/// config when there is one, then the tar trailer.
fn plan_spkb(
    id: &str,
    version: &str,
    version_dir: &Path,
    config: Option<&[u8]>,
) -> Result<Plan, String> {
    let mut segments = VecDeque::new();
    let mut size = 0u64;

    let backup = plugin::backup_json(id, version, config);
    push(
        &mut segments,
        &mut size,
        Segment::Bytes(tar_entry("backup.json", &backup, 0o644)?),
    );

    // The nested package is a tar stream of its own. Its headers, payload file
    // sends and padding are the bytes of the outer `plugin.spk` entry.
    let (inner, inner_size) = spk_segments(version_dir)?;
    push(
        &mut segments,
        &mut size,
        Segment::Bytes(tar_header("plugin.spk", inner_size, 0o644)?),
    );
    for part in inner {
        push(&mut segments, &mut size, part);
    }
    push(
        &mut segments,
        &mut size,
        Segment::Bytes(tar_padding(inner_size)?),
    );

    if let Some(config) = config {
        push(
            &mut segments,
            &mut size,
            Segment::Bytes(tar_entry("config", config, 0o600)?),
        );
    }

    push(&mut segments, &mut size, Segment::Bytes(vec![0u8; TRAILER]));
    Ok(Plan {
        head: download_head(id, version, "spkb", size),
        segments,
    })
}

/// The body of an installed `.spk`: the verbatim manifest and signature, then
/// each payload file in manifest order. Payload bytes are never read here.
fn spk_segments(version_dir: &Path) -> Result<(VecDeque<Segment>, u64), String> {
    let manifest_bytes = fs::read(version_dir.join("manifest.json"))
        .map_err(|error| format!("read manifest: {}", error))?;
    let manifest = package::parse_manifest(&manifest_bytes)?;
    let signature = package::read_export_signature(version_dir)?;
    let mut segments = VecDeque::new();
    let mut size = 0u64;
    push(
        &mut segments,
        &mut size,
        Segment::Bytes(tar_entry("manifest.json", &manifest_bytes, 0o644)?),
    );
    push(
        &mut segments,
        &mut size,
        Segment::Bytes(tar_entry("signature.json", &signature, 0o644)?),
    );
    let mut files = manifest.files.clone();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for file in files {
        let path = version_dir.join(&file.path);
        // `parse_manifest` validated the path; the size check is what keeps a
        // truncated install from advertising a Content-Length it cannot send.
        let metadata =
            fs::metadata(&path).map_err(|error| format!("stat {}: {}", file.path, error))?;
        if metadata.len() != file.size {
            return Err(format!("installed file is corrupt: {}", file.path));
        }
        push(
            &mut segments,
            &mut size,
            Segment::Bytes(tar_header(&file.path, file.size, file.mode & 0o777)?),
        );
        if file.size > 0 {
            push(
                &mut segments,
                &mut size,
                Segment::File {
                    path,
                    offset: 0,
                    length: file.size,
                },
            );
        }
        push(
            &mut segments,
            &mut size,
            Segment::Bytes(tar_padding(file.size)?),
        );
    }
    push(&mut segments, &mut size, Segment::Bytes(vec![0u8; TRAILER]));
    Ok((segments, size))
}

/// Account one segment. The running size is the response's Content-Length, so
/// every segment must be counted exactly once.
fn push(segments: &mut VecDeque<Segment>, size: &mut u64, part: Segment) {
    let bytes = match &part {
        Segment::Bytes(data) => data.len() as u64,
        Segment::File { length, .. } => *length,
    };
    *size = size.saturating_add(bytes);
    segments.push_back(part);
}

fn download_head(id: &str, version: &str, extension: &str, size: u64) -> Vec<u8> {
    let filename = format!(
        "{}-{}.{}",
        super::api::safe_filename(id),
        super::api::safe_filename(version),
        extension
    );
    http::response_head(
        200,
        "application/octet-stream",
        &[
            (
                "Content-Disposition".to_string(),
                format!("attachment; filename=\"{}\"", filename),
            ),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        size,
    )
}

fn tar_entry(path: &str, data: &[u8], mode: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(512 + data.len());
    package::write_tar_entry(&mut out, path, data, mode)?;
    Ok(out)
}

fn tar_header(path: &str, size: u64, mode: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(512);
    package::write_tar_header(&mut out, path, size, mode)?;
    Ok(out)
}

fn tar_padding(size: u64) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(512);
    package::write_tar_padding(&mut out, size)?;
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A scratch directory this test may write. The build tree is the one
    /// place that is always writable, including on a host where the system
    /// temp directory is locked down.
    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("export-unit")
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&dir)
            .unwrap_or_else(|error| panic!("temp root {}: {}", dir.display(), error));
        dir
    }

    /// An installed plugin version directory, laid out the way the installer
    /// leaves it: manifest, signature and payload files.
    fn installed(name: &str, files: &[(&str, &[u8], u32)]) -> PathBuf {
        let version_dir = temp_dir().join(name).join("1.0.0");
        fs::create_dir_all(&version_dir).expect("version dir");
        let mut entries = Vec::new();
        for (path, data, mode) in files {
            let target = version_dir.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("payload dir");
            }
            fs::write(&target, data).expect("payload");
            entries.push(json!({
                "path": path,
                "size": data.len(),
                "sha256": package::hex_sha256(data),
                "mode": mode,
            }));
        }
        let manifest = json!({
            "schema": 1,
            "name": name,
            "version": "1.0.0",
            "desc": "export unit test",
            "platform": package::target(),
            "protocol": 1,
            "exec": {"path": files[0].0},
            "capabilities": [],
            "files": entries,
            "ui": null,
            "hooks": null,
            "config": null,
            "configCheck": null,
        });
        fs::write(
            version_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("write manifest");
        fs::write(
            version_dir.join("signature.json"),
            br#"{"alg":"ed25519","keyId":"test","sig":"00"}"#,
        )
        .expect("write signature");
        version_dir
    }

    /// Drive the handler the way the server driver does: one bounded output
    /// buffer, server-side file sends performed, then a fresh Writable event.
    fn drive(mut handler: ExportStream, limit: usize) -> (Vec<u8>, CloseReason) {
        let mut all = Vec::new();
        let mut out = Vec::new();
        for _ in 0..100_000 {
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
                StreamAction::SubmitBlocking(_) => panic!("an export submits no blocking job"),
            }
        }
        panic!("the handler never finished");
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    fn body(bytes: &[u8]) -> &[u8] {
        let split = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response head terminator");
        &bytes[split + 4..]
    }

    fn error_of(bytes: &[u8]) -> Value {
        serde_json::from_slice(body(bytes)).expect("json error body")
    }

    fn spk(version_dir: &Path, name: &str) -> ExportStream {
        ExportStream::from_plan(plan_spk(name, "1.0.0", version_dir))
    }

    #[test]
    fn the_spk_body_is_the_legacy_archive_byte_for_byte() {
        let version_dir = installed(
            "demo",
            &[
                ("payload/run.sh", b"#!/bin/sh\necho hi\n", 0o755),
                ("payload/data.bin", &[7u8; 1300], 0o644),
                ("payload/empty", b"", 0o644),
            ],
        );
        let mut expected = Vec::new();
        package::write_installed_spk(&version_dir, &mut expected).expect("legacy writer");

        let (bytes, reason) = drive(spk(&version_dir, "demo"), 128);
        assert_eq!(reason, CloseReason::Completed);
        let head = text(&bytes);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: application/octet-stream\r\n"));
        assert!(head.contains(&format!(
            "Content-Disposition: attachment; filename=\"demo-1.0.0.spk\"\r\n"
        )));
        assert!(head.contains("Cache-Control: no-store\r\n"));
        assert!(
            head.contains(&format!("Content-Length: {}\r\n", expected.len())),
            "{head}"
        );
        assert_eq!(
            body(&bytes),
            expected.as_slice(),
            "the streamed archive must match the legacy writer byte for byte"
        );
    }

    #[test]
    fn the_spkb_body_is_backup_the_nested_spk_and_the_active_config() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        let config = b"good template\n";
        let (bytes, reason) = drive(
            ExportStream::from_plan(plan_spkb("demo", "1.0.0", &version_dir, Some(config))),
            64,
        );
        assert_eq!(reason, CloseReason::Completed);
        let head = text(&bytes);
        assert!(head.contains("filename=\"demo-1.0.0.spkb\""), "{head}");
        assert!(head.contains(&format!("Content-Length: {}\r\n", body(&bytes).len())));

        let entries = package::read_tar(body(&bytes)).expect("tar");
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, ["backup.json", "plugin.spk", "config"]);

        let backup: Value = serde_json::from_slice(&entries[0].data).expect("backup json");
        assert_eq!(backup["schema"], 1);
        assert_eq!(backup["id"], "demo");
        assert_eq!(backup["version"], "1.0.0");
        assert_eq!(backup["configSha256"], package::hex_sha256(config));
        assert!(backup["exportedAt"].as_u64().is_some());

        let mut nested = Vec::new();
        package::write_installed_spk(&version_dir, &mut nested).expect("nested spk");
        assert_eq!(
            entries[1].data, nested,
            "the plugin.spk entry is the signed package of the current version"
        );
        assert_eq!(entries[2].data, config);
        assert_eq!(entries[2].mode, 0o600, "the config keeps its private mode");
    }

    #[test]
    fn a_plugin_without_an_active_config_exports_without_a_config_entry() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        let (bytes, _) = drive(
            ExportStream::from_plan(plan_spkb("demo", "1.0.0", &version_dir, None)),
            512,
        );
        let entries = package::read_tar(body(&bytes)).expect("tar");
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names, ["backup.json", "plugin.spk"]);
        let backup: Value = serde_json::from_slice(&entries[0].data).expect("backup json");
        assert!(backup["configSha256"].is_null());
    }

    #[test]
    fn a_large_payload_is_a_server_side_send_instead_of_a_buffer() {
        let payload = vec![0x5a; 1024 * 1024];
        let version_dir = installed("big", &[("payload/blob.bin", &payload, 0o644)]);
        let mut handler = spk(&version_dir, "big");
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
                    assert_eq!(send.length, payload.len() as u64);
                    sends += 1;
                }
                StreamAction::Close(_) => break,
                _ => {}
            }
        }
        assert_eq!(sends, 1, "the payload is one server-side file send");
        assert!(
            head_bytes < 4096,
            "only headers, JSON entries and padding are buffered: {head_bytes}"
        );
    }

    #[test]
    fn an_unsigned_install_is_a_streamed_500_before_the_headers() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        fs::remove_file(version_dir.join("signature.json")).expect("drop signature");
        let (bytes, reason) = drive(spk(&version_dir, "demo"), 4096);
        assert_eq!(reason, CloseReason::Completed);
        assert!(
            text(&bytes).starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "{}",
            text(&bytes)
        );
        let message = error_of(&bytes)["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(message.contains("no signature"), "{message}");
    }

    #[test]
    fn a_truncated_payload_fails_before_the_headers() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        fs::write(version_dir.join("payload/run.sh"), b"").expect("truncate payload");
        let (bytes, _) = drive(spk(&version_dir, "demo"), 4096);
        let response = text(&bytes);
        assert!(
            response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "{response}"
        );
        let message = error_of(&bytes)["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(message.contains("installed file is corrupt"), "{message}");
    }

    #[test]
    fn an_unknown_plugin_is_a_streamed_404() {
        let (bytes, reason) = drive(
            ExportStream::open(ExportTarget::Spkb {
                id: "no-such-export-plugin".into(),
            }),
            4096,
        );
        assert_eq!(reason, CloseReason::Completed);
        assert!(text(&bytes).starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert_eq!(error_of(&bytes)["error"]["code"], "not_found");

        // A name that cannot be a plugin id is rejected before any path join.
        let (bytes, _) = drive(
            ExportStream::open(ExportTarget::Spk {
                id: "../escape".into(),
            }),
            4096,
        );
        assert!(text(&bytes).starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn only_get_export_paths_are_claimed() {
        match export_target("GET", "/api/v1/export/demo") {
            Some(ExportTarget::Spkb { id }) => assert_eq!(id, "demo"),
            _ => panic!("the .spkb route must be claimed"),
        }
        match export_target("GET", "/api/v1/export/demo/spk?download=1") {
            Some(ExportTarget::Spk { id }) => assert_eq!(id, "demo"),
            _ => panic!("the .spk route must be claimed"),
        }
        match export_target("GET", "/api/v1/export/a%2Fb") {
            Some(ExportTarget::Spkb { id }) => assert_eq!(id, "a/b"),
            _ => panic!("the id is percent-decoded like the legacy route"),
        }
        for (method, path) in [
            ("POST", "/api/v1/export/demo"),
            ("HEAD", "/api/v1/export/demo/spk"),
            ("GET", "/api/v1/export/demo/config"),
            ("GET", "/api/v1/export"),
            ("GET", "/api/v1/export/demo/spk/extra"),
        ] {
            assert!(
                export_target(method, path).is_none(),
                "{method} {path} must not be an export"
            );
        }
    }

    #[test]
    fn an_export_is_a_long_lived_stream() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        assert_eq!(
            spk(&version_dir, "demo").stream_class(),
            StreamClass::LongLived
        );
    }

    #[test]
    fn a_disconnect_releases_the_rest_of_the_plan() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        let mut handler = spk(&version_dir, "demo");
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 64));
        assert!(!out.is_empty(), "the head went out before the disconnect");
        assert!(matches!(step.action, StreamAction::NeedWrite));
        let mut discarded = Vec::new();
        let step = handler.on_event(
            StreamEvent::Closed(CloseReason::PeerClosed),
            &mut OutBuffer::new(&mut discarded, 0),
        );
        assert!(handler.segments.is_empty());
        assert!(handler.head.is_empty());
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
    }
}
