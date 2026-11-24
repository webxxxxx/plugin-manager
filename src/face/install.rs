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

/// The one success body both import routes answer with. The package/backup
/// route (`import_response`) and the bare-config route (`import_outcome_json`)
/// only project their input into these four values; the JSON keys, their order,
/// the summary-then-config-id fallback and the empty-warnings omission live
/// here and nowhere else (#247).
fn import_success(
    summary: Option<Value>,
    config_id: Option<String>,
    config_applied: bool,
    warnings: Vec<String>,
) -> Vec<u8> {
    let mut body = json!({
        "ok": true,
        "configApplied": config_applied,
    });
    if let Some(summary) = summary {
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

/// A daemon install status carries the summary under one key; both routes use
/// this projection so neither re-implements the lookup.
fn import_status_summary(status: &Value) -> Value {
    status.get("summary").cloned().unwrap_or(Value::Null)
}

/// The package and backup imports know the daemon status they just installed.
fn import_response(
    status: Value,
    config_id: Option<String>,
    config_applied: bool,
    warnings: Vec<String>,
) -> Vec<u8> {
    let summary = import_status_summary(&status);
    import_success(
        (!summary.is_null()).then_some(summary),
        config_id,
        config_applied,
        warnings,
    )
}

/// The bare-config import knows only a transfer outcome.
fn import_outcome_json(outcome: transfer::Outcome) -> Vec<u8> {
    let summary = outcome.status.as_ref().map(import_status_summary);
    let config_applied = outcome.config.is_some();
    import_success(summary, outcome.config, config_applied, outcome.warnings)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::platform::config::Config;
    use crate::platform::paths::Paths;
    use crate::server::auth::lan::Lan;
    use crate::server::http::model as http;
    use crate::server::http::{CloseReason, Exchange, StreamHandler};

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

    /// The HTTP body after the status line and headers; the assembler is the
    /// only thing under test, so the head must not mask a body difference.
    fn body_of(response: Vec<u8>) -> Vec<u8> {
        let end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response header terminator");
        response[end + 4..].to_vec()
    }

    /// #247: package, backup and bare-config imports project into the same four
    /// inputs, so equal inputs must answer byte-identical JSON from both
    /// assembly points.
    #[test]
    fn both_import_paths_answer_the_same_success_body() {
        let summary = json!({"id": "demo", "version": "1.0.0", "state": "stopped"});
        let cases = [
            // Package import: the summary, no config applied.
            (Some(summary.clone()), None, false, Vec::new()),
            // Backup import with a config and a warning.
            (
                Some(summary.clone()),
                Some("demo".to_string()),
                true,
                vec!["replaced an older installed version".to_string()],
            ),
            // Backup import with an empty warning list.
            (
                Some(summary.clone()),
                Some("demo".to_string()),
                true,
                Vec::new(),
            ),
            // Bare config import: no summary, only the config id.
            (None, Some("demo".to_string()), true, Vec::new()),
        ];
        for (summary, config_id, config_applied, warnings) in cases {
            let status = summary
                .map(|summary| json!({ "summary": summary }))
                .unwrap_or(Value::Null);
            let via_status = import_response(
                status.clone(),
                config_id.clone(),
                config_applied,
                warnings.clone(),
            );
            let via_outcome = import_outcome_json(transfer::Outcome {
                status: (!status.is_null()).then_some(status),
                config: config_id,
                warnings,
            });
            assert_eq!(
                body_of(via_outcome),
                body_of(via_status),
                "the two import paths must assemble the same body"
            );
        }
    }

    /// #247: the shared assembler keeps the wire contract: field order,
    /// summary-before-config-id priority and the empty-warnings omission.
    #[test]
    fn import_success_keeps_field_order_and_omits_empty_warnings() {
        let summary = json!({"id": "demo", "version": "1.0.0", "state": "stopped"});
        // serde_json keeps its default BTreeMap here, so the wire order is the
        // sorted key order; the assertion locks the bytes both routes send.
        assert_eq!(
            body_of(import_success(
                Some(summary.clone()),
                None,
                false,
                Vec::new()
            )),
            br#"{"configApplied":false,"id":"demo","ok":true,"state":"stopped","version":"1.0.0"}"#
                .to_vec()
        );
        // The summary id wins over the config id.
        assert_eq!(
            body_of(import_success(
                Some(summary),
                Some("other".to_string()),
                true,
                vec!["older version replaced".to_string()],
            )),
            br#"{"configApplied":true,"id":"demo","ok":true,"state":"stopped","version":"1.0.0","warnings":["older version replaced"]}"#
                .to_vec()
        );
        // The bare-config import falls back to the config id.
        assert_eq!(
            body_of(import_success(
                None,
                Some("demo".to_string()),
                true,
                Vec::new(),
            )),
            br#"{"configApplied":true,"id":"demo","ok":true}"#.to_vec()
        );
        // A transfer status that is present without a summary keeps the old
        // null fields; it must not fall back to the config id.
        assert_eq!(
            body_of(import_success(
                Some(Value::Null),
                Some("demo".to_string()),
                false,
                Vec::new(),
            )),
            br#"{"configApplied":false,"id":null,"ok":true,"state":null,"version":null}"#.to_vec()
        );
    }

    fn test_config() -> Arc<Config> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::platform::test_support::temp_dir().join(format!(
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

    /// A configuration whose derived `max_package_bytes` is the floor (1 MiB),
    /// so a 2 MiB body is over the line on every machine.
    fn small_package_config() -> Arc<Config> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::platform::test_support::temp_dir().join(format!(
            "svcd-install-small-{}-{}",
            std::process::id(),
            nanos
        ));
        let settings = crate::platform::settings::Settings {
            max_package_bytes: Some(1024 * 1024),
            ..crate::platform::settings::Settings::default()
        };
        Arc::new(Config::with_paths(settings, None, Paths::under(&root)))
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

    fn drive(
        mut handler: Box<dyn StreamHandler>,
        body: &[u8],
        chunk: usize,
    ) -> (Vec<u8>, CloseReason, usize) {
        crate::server::http::test_support::drive_body(&mut *handler, 4096, body, chunk)
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

    /// #286: a package over `max_package_bytes` is refused before the body is
    /// read, so the old code answered and closed while the client was still
    /// sending. The kernel turned that into an RST and the browser showed
    /// `TypeError: Failed to fetch`. The refusal must answer *and* drain.
    #[test]
    fn an_over_limit_body_is_answered_413_and_drained() {
        let over = 2 * 1024 * 1024usize;
        for (path, message) in [
            ("/api/v1/plugins", "package is too large"),
            ("/api/v1/import", "import is too large"),
        ] {
            let head = parse_head(
                format!(
                    "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: {over}\r\n\r\n"
                )
                .as_bytes(),
            );
            let mut handler =
                match exchange(&head, &lan(), small_package_config()).expect("streamed exchange") {
                    Exchange::Stream(handler) => handler,
                    Exchange::Response(_) => panic!("a package route must stream"),
                };
            let body = vec![0u8; over];
            let (response, reason, consumed) = crate::server::http::test_support::drive_body(
                &mut *handler,
                4096,
                &body,
                64 * 1024,
            );
            assert_eq!(reason, CloseReason::Completed, "{path}");
            assert_eq!(
                consumed,
                body.len(),
                "{path}: the refused body must be drained, not reset"
            );
            let text = String::from_utf8_lossy(&response);
            assert!(
                text.starts_with("HTTP/1.1 413 Payload Too Large"),
                "{path}: {text}"
            );
            assert!(text.contains(message), "{path}: {text}");
            assert!(
                text.contains("\"code\":\"payload_too_large\""),
                "{path}: {text}"
            );
        }
    }

    /// #288: the package bytes the store imported into temp as a blob (its
    /// `package_blob` hands the browser a `svdc://blob/...` link) come back as
    /// the install body. When the attempt ends — here it is refused because no
    /// daemon answers — that entry must be released instead of pinning 15 MiB
    /// on the volume for `temp_retention_secs`.
    #[test]
    fn an_install_releases_the_temp_blob_it_consumed() {
        let config = test_config();
        let body =
            crate::manage::test_support::test_signed_package("demo", crate::platform::target());
        let staged = crate::platform::storage::put(&config.paths, &body).expect("stage the blob");
        let unrelated =
            crate::platform::storage::put(&config.paths, b"not this install").expect("other blob");

        let head = parse_head(
            format!(
                "POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        let handler = match exchange(&head, &lan(), config.clone()).expect("streamed exchange") {
            Exchange::Stream(handler) => handler,
            Exchange::Response(_) => panic!("a package route must stream"),
        };
        let (response, reason, consumed) = drive(handler, &body, 64 * 1024);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, body.len(), "the whole body is the package");
        assert!(
            !String::from_utf8_lossy(&response).starts_with("HTTP/1.1 2"),
            "this unit test has no daemon to install into: {}",
            String::from_utf8_lossy(&response)
        );
        let staged_path = crate::platform::storage::locate(&config.paths, &staged.id)
            .expect("staged blob path");
        assert!(
            !staged_path.exists(),
            "the installed package must release its temp blob"
        );
        let unrelated_path =
            crate::platform::storage::locate(&config.paths, &unrelated.id).expect("other path");
        assert!(
            unrelated_path.exists(),
            "another transfer's blob is not ours to delete"
        );
    }

    #[test]
    fn a_full_blocking_queue_turns_into_503_busy() {
        let mut handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\n\r\n",
        );
        let out = crate::server::http::test_support::drive_busy(&mut *handler, b"body", 4096);
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
