//! /api/v1/plugins and /api/v1/import as sans-IO streams.
//!
//! Both endpoints used to take the connection out of the server loop and spawn
//! a per-request thread. Now the handler receives body chunks, feeds them into
//! a staging installer (or a bounded config buffer) through the blocking pool,
//! and answers once. A slow or disconnected client pauses input while one chunk
//! is in flight, so staging never grows an unbounded queue.
//!
//! The install semantics live where they already did: package::StagingInstaller
//! validates the manifest, the signature, files[] and the staged payload; this
//! face only decides the route, the framing and the response shape.
//!
//! The mechanics are split by responsibility: `upload` owns route recognition,
//! framing and the stream handler; `install_flow` and `import_flow` own the two
//! body states; `backup` owns the .spkb sink. This entry module keeps the
//! staged-install handoff and the response shapes both flows share.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::manage::ops::transfer;
use crate::platform::config::Config;
use crate::platform::paths::Paths;
use crate::server::http::model as http;

use super::support::{self, CallBudget};

mod backup;
mod import_flow;
mod install_flow;
mod upload;

#[allow(unused_imports)]
// The entry surface: the install-face tests construct the handler directly.
pub use upload::{exchange, target, PackageTarget, PackageUploadStream};

/// Send one staged package tree to the daemon. A refusal removes the tree,
/// so a failed install never leaves staging behind no matter which route
/// (install, .spk import, .spkb import) produced it.
fn install_staged(
    config: &Config,
    root: &Path,
    id: &str,
    version: &str,
    strategy: &str,
    allow_hooks: bool,
    budget: &CallBudget,
) -> Result<Value, Vec<u8>> {
    support::rpc(
        config.paths.control_dir(),
        "plugin.install_staged",
        json!({
            "path": root.to_string_lossy(),
            "id": id,
            "version": version,
            "strategy": strategy,
            "allowHooks": allow_hooks,
        }),
        budget,
    )
    .inspect_err(|_| {
        let _ = std::fs::remove_dir_all(root);
    })
}

fn staging_path(paths: &Paths) -> PathBuf {
    let plugins = paths
        .plugins_dir
        .canonicalize()
        .unwrap_or_else(|_| paths.plugins_dir.clone());
    let _ = std::fs::create_dir_all(&plugins);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    plugins.join(format!(".staging-{}-{}", std::process::id(), nanos))
}

fn import_response(
    status: Value,
    config_id: Option<String>,
    config_applied: bool,
    warnings: Vec<String>,
) -> Vec<u8> {
    let summary = status.get("summary").cloned().unwrap_or(Value::Null);
    let mut body = json!({
        "ok": true,
        "configApplied": config_applied,
    });
    if !summary.is_null() {
        body["id"] = summary.get("id").cloned().unwrap_or(Value::Null);
        body["version"] = summary.get("version").cloned().unwrap_or(Value::Null);
        body["state"] = summary.get("state").cloned().unwrap_or(Value::Null);
    } else if let Some(id) = config_id {
        body["id"] = json!(id);
    }
    if !warnings.is_empty() {
        body["warnings"] = json!(warnings);
    }
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn import_outcome_json(outcome: transfer::Outcome) -> Vec<u8> {
    let mut body = json!({
        "ok": true,
        "configApplied": outcome.config.is_some(),
    });
    if let Some(status) = outcome.status {
        let summary = &status["summary"];
        body["id"] = summary.get("id").cloned().unwrap_or(Value::Null);
        body["version"] = summary.get("version").cloned().unwrap_or(Value::Null);
        body["state"] = summary.get("state").cloned().unwrap_or(Value::Null);
    } else if let Some(id) = outcome.config {
        body["id"] = json!(id);
    }
    if !outcome.warnings.is_empty() {
        body["warnings"] = json!(outcome.warnings);
    }
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::platform::config::Config;
    use crate::platform::paths::Paths;
    use crate::server::auth::lan::Lan;
    use crate::server::http::model as http;
    use crate::server::http::{
        BlockingOutcome, CloseReason, Exchange, OutBuffer, StreamAction, StreamEvent, StreamHandler,
    };

    use super::*;

    fn parse_head(request: &[u8]) -> http::Head {
        http::parse_head(request).expect("request head").0
    }

    fn target_of(method: &str, path: &str) -> Option<PackageTarget> {
        target(&parse_head(
            format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
        ))
    }

    #[test]
    fn only_post_package_routes_are_claimed() {
        assert!(matches!(
            target_of("POST", "/api/v1/plugins"),
            Some(PackageTarget::Install { .. })
        ));
        assert!(matches!(
            target_of("POST", "/api/v1/import?config_id=demo"),
            Some(PackageTarget::Import { .. })
        ));
        for (method, path) in [
            ("GET", "/api/v1/plugins"),
            ("GET", "/api/v1/import"),
            ("PUT", "/api/v1/plugins"),
            ("POST", "/api/v1/plugins/demo"),
            ("POST", "/api/v1/plugins/demo/ui-token"),
        ] {
            assert!(target_of(method, path).is_none(), "{method} {path}");
        }
    }

    #[test]
    fn the_install_query_keeps_its_defaults() {
        let Some(PackageTarget::Install {
            strategy,
            enable,
            allow_hooks,
        }) = target_of("POST", "/api/v1/plugins")
        else {
            panic!("install route");
        };
        assert_eq!(strategy, "replace");
        assert!(enable);
        assert!(!allow_hooks);

        let Some(PackageTarget::Install {
            strategy,
            enable,
            allow_hooks,
        }) = target_of(
            "POST",
            "/api/v1/plugins?strategy=keep-old&enable=false&allow_hooks=true",
        )
        else {
            panic!("install route");
        };
        assert_eq!(strategy, "keep-old");
        assert!(!enable);
        assert!(allow_hooks);
    }

    #[test]
    fn the_import_query_is_parsed_once() {
        let Some(PackageTarget::Import {
            config_id,
            no_config,
            force,
        }) = target_of(
            "POST",
            "/api/v1/import?config_id=demo&no_config=true&force=true",
        )
        else {
            panic!("import route");
        };
        assert_eq!(config_id.as_deref(), Some("demo"));
        assert!(no_config);
        assert!(force);
    }

    fn test_config() -> Arc<Config> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-install-face-{}-{}",
            std::process::id(),
            nanos
        ));
        Arc::new(Config::with_paths(
            crate::platform::settings::Settings::default(),
            None,
            Paths::under(&root),
        ))
    }

    fn lan() -> Arc<Mutex<Lan>> {
        use std::net::SocketAddr;
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
        Arc::new(Mutex::new(Lan::new(bind, bind, false, None)))
    }

    fn stream_handler(request: &[u8]) -> Box<dyn StreamHandler> {
        let head = parse_head(request);
        match exchange(&head, &lan(), test_config()).expect("streamed exchange") {
            Exchange::Stream(handler) => handler,
            Exchange::Response(_) => panic!("a package route must stream"),
        }
    }

    /// Drive the handler the way the server does: write, read body chunks, run
    /// submitted jobs inline, then write the final response. Returns the bytes
    /// written, the close reason and how much of the body the handler consumed.
    fn drive(
        mut handler: Box<dyn StreamHandler>,
        body: &[u8],
        chunk: usize,
    ) -> (Vec<u8>, CloseReason, usize) {
        let limit = 4096;
        let mut out = Vec::new();
        let mut all = Vec::new();
        let mut offset = 0;
        let mut step =
            handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
        loop {
            all.extend_from_slice(&out);
            out.clear();
            match step.action {
                StreamAction::Close(reason) => return (all, reason, offset),
                StreamAction::SubmitBlocking(job) => {
                    let outcome = job(crate::server::http::JobBudget::for_test());
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
                        panic!("handler still wants body after {} bytes", offset);
                    }
                    let end = (offset + chunk).min(body.len());
                    step = handler.on_event(
                        StreamEvent::Data(&body[offset..end]),
                        &mut OutBuffer::new(&mut out, limit),
                    );
                    offset = end;
                }
                StreamAction::SendFile(_) => panic!("a package upload sends no file"),
            }
        }
    }

    #[test]
    fn a_wrong_content_type_answers_then_consumes_the_body() {
        let handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\nContent-Length: 7\r\n\r\n",
        );
        let (response, reason, consumed) = drive(handler, b"ignored", 3);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, 7, "the refused body must still be drained");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 Bad Request"), "{text}");
        assert!(
            text.contains("Content-Type must be application/octet-stream"),
            "{text}"
        );
    }

    #[test]
    fn a_full_blocking_queue_turns_into_503_busy() {
        let mut handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\n\r\n",
        );
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        assert!(matches!(step.action, StreamAction::NeedRead));
        let step = handler.on_event(
            StreamEvent::Data(b"body"),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(step.action, StreamAction::SubmitBlocking(_)));
        let step = handler.on_event(
            StreamEvent::BlockingDone(BlockingOutcome::Busy),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(step.action, StreamAction::Close(_)));
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{text}"
        );
        assert!(text.contains("Retry-After: 1\r\n"), "{text}");
        assert!(text.contains("\"code\":\"busy\""), "{text}");
    }

    #[test]
    fn an_over_limit_chunked_body_is_refused_with_413() {
        let handler = PackageUploadStream::new(
            PackageTarget::Install {
                strategy: "replace".into(),
                enable: true,
                allow_hooks: false,
            },
            http::Framing::Chunked,
            false,
            test_config(),
            8,
        );
        // Two 5-byte chunks exceed the 8-byte decoded limit.
        let body = b"5\r\nabcde\r\n5\r\nfghij\r\n0\r\n\r\n";
        let (response, reason, _) = drive(Box::new(handler), body, 3);
        assert_eq!(reason, CloseReason::Completed);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 413 Payload Too Large"), "{text}");
        assert!(text.contains("\"code\":\"payload_too_large\""), "{text}");
    }

    #[test]
    fn an_install_error_drains_the_rest_of_the_body() {
        let handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: 612\r\n\r\n",
        );
        // 0xAA is not a valid tar header: the feed fails at the first 512-byte
        // boundary, after the handler has already promised a response.
        let body = vec![0xAAu8; 612];
        let (response, reason, consumed) = drive(handler, &body, 32);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, body.len(), "the refused body must be drained");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 Bad Request"), "{text}");
    }

    #[test]
    fn a_broken_chunk_after_a_refusal_does_not_write_a_second_response() {
        let handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        let mut body = Vec::new();
        body.extend_from_slice(b"200\r\n");
        body.extend_from_slice(&[0xAAu8; 512]);
        body.extend_from_slice(b"\r\n");
        // A size line that is not hex: the decoder cannot find a body boundary
        // after the first chunk already produced a 400.
        body.extend_from_slice(b"zz\r\n");
        let (response, reason, consumed) = drive(handler, &body, 17);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, body.len());
        let text = String::from_utf8_lossy(&response);
        assert_eq!(
            text.matches("HTTP/1.1 ").count(),
            1,
            "only one response head may be written: {text}"
        );
        assert!(text.starts_with("HTTP/1.1 400 Bad Request"), "{text}");
    }
}
