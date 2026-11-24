//! `/api/v1/export/{id}` (`.spkb`) and `.../{id}/spk` as a sans-IO stream.
//!
//! The response body is the same tar the legacy takeover produced: the same
//! entry order, tar headers, padding and trailer. What changed is who produces
//! it. The archive layout lives in `package` (the format) and `plugin` (the
//! plugin layout), which hand over a plan of byte parts and payload files. This
//! face only projects that plan onto the response: it writes the small parts
//! into the bounded output buffer and asks the server to send each payload
//! file. No archive is buffered whole, no temporary file is written, and no
//! thread is created per export.
//!
//! The plan is built on the first writable event, after the server admitted the
//! stream, so a request refused with 503 busy never pays for it.
//!
//! One deliberate difference from the byte-verifying writer the CLI still uses
//! (`TarPlan::write_verified`): a streamed export hands each payload straight
//! to the kernel, so the handler never hashes those bytes. The plan still
//! rejects a missing or truncated payload before the headers go out, and the
//! install path verifies `files[].sha256` again on import (#120).

use std::fs::File;
use std::sync::Arc;

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::{TarPart, TarPlan};
use crate::platform::paths::Paths;
use crate::server::http::model as http;
use crate::server::http::{
    CloseReason, FileSend, OutBuffer, StreamAction, StreamClass, StreamEvent, StreamHandler,
    StreamStep,
};

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

/// One export being streamed.
///
/// The plan is built on the first writable event, after the server admitted
/// the stream, so a request refused with 503 busy never pays for it.
pub struct ExportStream {
    paths: Arc<Paths>,
    pending: Option<Pending>,
    head: Vec<u8>,
    head_sent: usize,
    plan: TarPlan,
    index: usize,
    /// How much of the current byte part is already written.
    at: usize,
}

/// 404 when nothing is installed; 409 with the recorded root when the plugin
/// exists but its volume is not there. The difference is what tells an operator
/// to plug a card back in instead of re-installing (ADR-0030).
fn missing_plugin(paths: &Paths, id: &str) -> Vec<u8> {
    match crate::platform::locations::root_of(paths, id) {
        Some(root) if !crate::platform::locations::available(&root) => http::json_error(
            409,
            "root_unavailable",
            &format!(
                "plugin {} lives in {}, which is not available right now; put the volume back to export it",
                id,
                root.display()
            ),
        ),
        _ => http::json_error(404, "not_found", &format!("unknown plugin: {}", id)),
    }
}

struct Pending {
    target: ExportTarget,
    version: String,
}

impl ExportStream {
    pub fn open(target: ExportTarget, paths: Arc<Paths>) -> ExportStream {
        let id = target.id();
        // Validate before touching the filesystem: plugin_dir joins the id.
        let version = if crate::kernel::registry::valid_id(id) {
            plugin::current_version(&paths, id)
        } else {
            None
        };
        match version {
            Some(version) => ExportStream {
                paths,
                pending: Some(Pending { target, version }),
                head: Vec::new(),
                head_sent: 0,
                plan: TarPlan::new(),
                index: 0,
                at: 0,
            },
            None => ExportStream::response(missing_plugin(&paths, id), paths),
        }
    }

    /// A complete response that is not an archive. The route is streamed, so
    /// even a failure is a stream response.
    fn response(head: Vec<u8>, paths: Arc<Paths>) -> ExportStream {
        ExportStream {
            paths,
            pending: None,
            head,
            head_sent: 0,
            plan: TarPlan::new(),
            index: 0,
            at: 0,
        }
    }

    /// Stream an already-built plan. Production builds its plan lazily in
    /// ExportStream::begin; this constructor is the test seam.
    #[cfg(test)]
    fn planned(
        filename: &str,
        plan: Result<TarPlan, ProblemError>,
        paths: Arc<Paths>,
    ) -> ExportStream {
        match plan {
            Ok(plan) => ExportStream {
                paths,
                pending: None,
                head: download_head(filename, plan.size()),
                head_sent: 0,
                plan,
                index: 0,
                at: 0,
            },
            Err(error) => {
                ExportStream::response(http::json_error(500, "internal", error.detail()), paths)
            }
        }
    }

    /// Build the archive plan now that the stream holds a budget slot.
    fn begin(&mut self) {
        let Some(Pending { target, version }) = self.pending.take() else {
            return;
        };
        let id = target.id().to_string();
        let version_dir = crate::kernel::layout::plugin_dir(&self.paths, &id).join(&version);
        let extension = match &target {
            ExportTarget::Spk { .. } => "spk",
            ExportTarget::Spkb { .. } => "spkb",
        };
        let filename = format!(
            "{}-{}.{}",
            super::reply::safe_filename(&id),
            super::reply::safe_filename(&version),
            extension
        );
        let plan = match target {
            ExportTarget::Spk { .. } => package::plan_installed_spk(&version_dir),
            ExportTarget::Spkb { .. } => match active_config(&self.paths, &id) {
                Ok(config) => {
                    plugin::plan_export_spkb(&id, &version, &version_dir, config.as_deref())
                }
                Err(error) => Err(error),
            },
        };
        let (head, plan) = match plan {
            Ok(plan) => (download_head(&filename, plan.size()), plan),
            Err(error) => (
                http::json_error(500, "internal", error.detail()),
                TarPlan::new(),
            ),
        };
        self.head = head;
        self.head_sent = 0;
        self.plan = plan;
        self.index = 0;
        self.at = 0;
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        self.begin();
        loop {
            if self.head_sent < self.head.len() && !drain(out, &self.head, &mut self.head_sent) {
                return StreamStep::wait(StreamAction::NeedWrite);
            }
            let Some(part) = self.plan.parts().get(self.index) else {
                return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
            };
            match part {
                TarPart::Bytes(data) => {
                    if !drain(out, data, &mut self.at) {
                        return StreamStep::wait(StreamAction::NeedWrite);
                    }
                    self.index += 1;
                    self.at = 0;
                }
                TarPart::File { path, length, .. } => {
                    let file = match File::open(path) {
                        Ok(file) => file,
                        Err(error) => {
                            return StreamStep::wait(StreamAction::Close(CloseReason::Failed(
                                format!("open {}: {}", path.display(), error),
                            )))
                        }
                    };
                    self.index += 1;
                    self.at = 0;
                    return StreamStep::wait(StreamAction::SendFile(FileSend {
                        file,
                        offset: 0,
                        length: *length,
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
                // A disconnect drops the pending work and the plan, so no file
                // handle or buffered piece outlives the connection.
                self.pending = None;
                self.plan = TarPlan::new();
                self.head.clear();
                self.head_sent = 0;
                self.index = 0;
                self.at = 0;
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

/// The active config of a .spkb backup; a plugin without one exports without
/// the config entry.
fn active_config(paths: &Paths, id: &str) -> Result<Option<Vec<u8>>, ProblemError> {
    match plugin::active_config(paths, id) {
        Some(path) => Ok(Some(std::fs::read(&path).map_err(|error| {
            Problem::Internal.error(format!("read config: {}", error))
        })?)),
        None => Ok(None),
    }
}

/// Append the not-yet-written tail of source, advancing at. Returns whether all
/// of it fit: a full output buffer makes this false.
fn drain(out: &mut OutBuffer<'_>, source: &[u8], at: &mut usize) -> bool {
    let take = (source.len() - *at).min(out.remaining());
    out.write(&source[*at..*at + take]);
    *at += take;
    *at == source.len()
}

fn download_head(filename: &str, size: u64) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;
    use std::path::{Path, PathBuf};
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
            "id": name,
            "name": name,
            "version": "1.0.0",
            "desc": "export unit test",
            "platform": crate::platform::target(),
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
        crate::server::http::test_support::drive_stream(
            &mut handler,
            limit,
            crate::server::http::test_support::SendFilePolicy::Continue,
            false,
        )
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

    /// The same archive composed through the pre-existing write_tar helper:
    /// an expectation independent of the plan's own part list.
    fn reference_spk(version_dir: &Path) -> Vec<u8> {
        let manifest = fs::read(version_dir.join("manifest.json")).expect("manifest");
        let signature = fs::read(version_dir.join("signature.json")).expect("signature");
        let parsed: Value = serde_json::from_slice(&manifest).expect("manifest json");
        let mut files: Vec<(String, u32)> = parsed["files"]
            .as_array()
            .expect("manifest files")
            .iter()
            .map(|file| {
                (
                    file["path"].as_str().expect("path").to_string(),
                    file["mode"].as_u64().expect("mode") as u32,
                )
            })
            .collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let payloads: Vec<(String, Vec<u8>, u32)> = files
            .into_iter()
            .map(|(path, mode)| {
                let data = fs::read(version_dir.join(&path)).expect("payload");
                (path, data, mode & 0o777)
            })
            .collect();
        let mut entries: Vec<(&str, &[u8], u32)> = vec![
            ("manifest.json", &manifest, 0o644),
            ("signature.json", &signature, 0o644),
        ];
        for (path, data, mode) in &payloads {
            entries.push((path.as_str(), data.as_slice(), *mode));
        }
        package::write_tar(&entries).expect("reference tar")
    }

    fn test_paths() -> Arc<Paths> {
        Arc::new(crate::platform::test_support::paths("export"))
    }

    fn spk(version_dir: &Path, name: &str) -> ExportStream {
        ExportStream::planned(
            &format!("{}-1.0.0.spk", name),
            package::plan_installed_spk(version_dir),
            test_paths(),
        )
    }

    fn spkb(version_dir: &Path, name: &str, config: Option<&[u8]>) -> ExportStream {
        ExportStream::planned(
            &format!("{}-1.0.0.spkb", name),
            plugin::plan_export_spkb(name, "1.0.0", version_dir, config),
            test_paths(),
        )
    }

    #[test]
    fn the_spk_body_matches_the_reference_archive_byte_for_byte() {
        let version_dir = installed(
            "demo",
            &[
                ("payload/run.sh", b"#!/bin/sh\necho hi\n", 0o755),
                ("payload/data.bin", &[7u8; 1300], 0o644),
                ("payload/empty", b"", 0o644),
            ],
        );
        let expected = reference_spk(&version_dir);

        let (bytes, reason) = drive(spk(&version_dir, "demo"), 128);
        assert_eq!(reason, CloseReason::Completed);
        let head = text(&bytes);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
        assert!(head.contains("Content-Type: application/octet-stream\r\n"));
        assert!(head.contains("Content-Disposition: attachment; filename=\"demo-1.0.0.spk\"\r\n"));
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
        let (bytes, reason) = drive(spkb(&version_dir, "demo", Some(config)), 64);
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

        assert_eq!(
            entries[1].data,
            reference_spk(&version_dir),
            "the plugin.spk entry is the signed package of the current version"
        );
        assert_eq!(entries[2].data, config);
    }

    #[test]
    fn a_plugin_without_an_active_config_exports_without_a_config_entry() {
        let version_dir = installed("demo", &[("payload/run.sh", b"#!/bin/sh\n", 0o755)]);
        let (bytes, _) = drive(spkb(&version_dir, "demo", None), 512);
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
            ExportStream::open(
                ExportTarget::Spkb {
                    id: "no-such-export-plugin".into(),
                },
                test_paths(),
            ),
            4096,
        );
        assert_eq!(reason, CloseReason::Completed);
        assert!(text(&bytes).starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert_eq!(error_of(&bytes)["error"]["code"], "not_found");

        // A name that cannot be a plugin id is rejected before any path join.
        let (bytes, _) = drive(
            ExportStream::open(
                ExportTarget::Spk {
                    id: "../escape".into(),
                },
                test_paths(),
            ),
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
        assert!(handler.pending.is_none());
        assert_eq!(handler.plan.size(), 0);
        assert!(handler.head.is_empty());
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
    }
}
