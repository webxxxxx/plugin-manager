//! Plugin Web UI: safe path resolution, ETag/304, and HTML base injection.
//!
//! svcd serves a plugin's `payload/web/` tree under `/plugin-ui/{id}/`. The
//! plugin never starts its own HTTP listener; `/plugin-ui/{id}/rpc` forwards a
//! JSON-RPC request to the plugin's stdio channel (see `bridge_request`), and
//! `uichannel` mirrors the plugin's notifications to SSE/WebSocket clients.

use serde_json::Value;

use crate::package;
use crate::rpc;

/// The page-facing JSON-RPC bridge for `/plugin-ui/{id}/rpc`.
///
/// The page speaks the same JSON-RPC 2.0 envelope as the plugin stdio channel;
/// only `tools/call` is bridged, with the plugin id supplying the namespace
/// prefix the daemon expects. Everything else is rejected locally.
#[derive(Debug)]
pub enum Bridge {
    /// Forward the call to the daemon as `registry.call`.
    Forward {
        id: Option<Value>,
        name: String,
        arguments: Value,
    },
    /// Answer the page with this JSON-RPC error; nothing reaches the plugin.
    Reject(rpc::Response),
}

/// Validate a page request and turn it into a `registry.call` the daemon can
/// route to the plugin. The daemon's response id is rewritten by the caller, so
/// the page sees its own id echoed back.
pub fn bridge_request(plugin: &str, body: &[u8]) -> Bridge {
    let Ok(text) = std::str::from_utf8(body) else {
        return Bridge::Reject(rpc::err(None, -32700, "request body is not UTF-8"));
    };
    let request = match rpc::parse(text) {
        Ok(request) => request,
        Err(response) => return Bridge::Reject(response),
    };
    let id = request.id.clone();
    if request.method != "tools/call" {
        return Bridge::Reject(rpc::err(
            id,
            -32601,
            "method not found; /rpc only bridges tools/call",
        ));
    }
    let Some(name) = request.params.get("name").and_then(Value::as_str) else {
        return Bridge::Reject(rpc::err(id, -32602, "tools/call requires params.name"));
    };
    let arguments = match request.params.get("arguments") {
        None | Some(Value::Null) => serde_json::json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => {
            return Bridge::Reject(rpc::err(
                id,
                -32602,
                "tools/call arguments must be an object",
            ))
        }
    };
    let prefix = format!("{}.", plugin);
    let name = if name.starts_with(&prefix) {
        name.to_string()
    } else {
        format!("{}{}", prefix, name)
    };
    Bridge::Forward {
        id,
        name,
        arguments,
    }
}

/// Resolve the URL sub-path after `/plugin-ui/{id}` to a path relative to the
/// plugin's `payload/web/` root.
///
/// An empty path (or a bare `/`) serves the manifest entry. A trailing slash
/// under a directory serves that directory's `index.html`. Percent escapes are
/// decoded before validation; `.`, `..`, empty segments and backslashes are
/// rejected, so the result can never leave the web root.
pub fn relative_path(subpath: &str, entry: &str) -> Option<String> {
    let raw = subpath.strip_prefix('/').unwrap_or(subpath);
    if raw.is_empty() {
        return entry_path(entry);
    }
    let decoded = percent_decode(raw)?;
    let mut path = decoded.as_str();
    let directory = path.ends_with('/');
    if directory {
        path = &path[..path.len() - 1];
        if path.is_empty() {
            return entry_path(entry);
        }
    }
    package::check_path(path).ok()?;
    if directory {
        Some(format!("{}/index.html", path))
    } else {
        Some(path.to_string())
    }
}

/// A strong ETag from the file's modification time and size. Changing the file
/// (even within the same second, thanks to sub-second precision) invalidates it.
pub fn etag(modified: std::time::SystemTime, size: u64) -> String {
    let elapsed = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "\"{}.{}-{}\"",
        elapsed.as_secs(),
        elapsed.subsec_nanos(),
        size
    )
}

/// Whether an `If-None-Match` header selects the current representation.
/// Weak validators and comma-separated lists are accepted; `*` matches anything.
pub fn not_modified(if_none_match: Option<&str>, etag: &str) -> bool {
    let Some(header) = if_none_match else {
        return false;
    };
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*"
            || candidate == etag
            || candidate.strip_prefix("W/").map(str::trim) == Some(etag)
    })
}

/// Insert `<base href="/plugin-ui/{id}/">` so a plugin's relative assets and
/// `fetch('rpc')` resolve under its own prefix.
///
/// The tag goes right after a leading doctype (inserting before it would drop
/// the document into quirks mode); without one it goes at the very start. A
/// doctype-lookalike in later content is not considered.
pub fn inject_base(html: &[u8], id: &str) -> Vec<u8> {
    let tag = format!("<base href=\"/plugin-ui/{}/\">", id);
    let at = doctype_end(html).unwrap_or(0);
    let mut out = Vec::with_capacity(html.len() + tag.len() + 2);
    out.extend_from_slice(&html[..at]);
    if at > 0 {
        out.push(b'\n');
    }
    out.extend_from_slice(tag.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(&html[at..]);
    out
}

fn doctype_end(html: &[u8]) -> Option<usize> {
    let mut index = 0;
    if html.starts_with(&[0xEF, 0xBB, 0xBF]) {
        index = 3;
    }
    while index < html.len() && html[index].is_ascii_whitespace() {
        index += 1;
    }
    let rest = &html[index..];
    const PREFIX: &[u8] = b"<!doctype";
    if rest.len() < PREFIX.len() || !rest[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    rest.iter()
        .position(|byte| *byte == b'>')
        .map(|pos| index + pos + 1)
}

fn entry_path(entry: &str) -> Option<String> {
    package::check_path(entry).ok()?;
    Some(entry.to_string())
}

/// Decode `%XX` escapes without treating `+` as a space (that rule belongs to
/// query strings, not paths). Invalid escapes or non-UTF-8 results are rejected
/// rather than guessed at.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn etag_is_mtime_plus_size_and_quoted() {
        // Windows SystemTime has 100 ns resolution, so use a microsecond multiple.
        let modified =
            UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_micros(123_456);
        assert_eq!(etag(modified, 42), "\"1700000000.123456000-42\"");
    }

    #[test]
    fn valid_tools_call_is_forwarded_with_the_plugin_prefix() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"status","arguments":{"level":2}}}"#;
        match bridge_request("demo", body) {
            Bridge::Forward {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, Some(serde_json::json!(7)));
                assert_eq!(name, "demo.status");
                assert_eq!(arguments, serde_json::json!({"level": 2}));
            }
            Bridge::Reject(response) => panic!("rejected: {:?}", response.error),
        }
    }

    #[test]
    fn an_already_prefixed_name_is_not_prefixed_twice() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"demo.status"}}"#;
        match bridge_request("demo", body) {
            Bridge::Forward { name, .. } => assert_eq!(name, "demo.status"),
            Bridge::Reject(response) => panic!("rejected: {:?}", response.error),
        }
    }

    #[test]
    fn another_plugins_prefix_cannot_escape_this_bridge() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"other.status"}}"#;
        match bridge_request("demo", body) {
            Bridge::Forward { name, .. } => assert_eq!(name, "demo.other.status"),
            Bridge::Reject(response) => panic!("rejected: {:?}", response.error),
        }
    }

    #[test]
    fn missing_arguments_default_to_an_empty_object() {
        let body = br#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"status"}}"#;
        match bridge_request("demo", body) {
            Bridge::Forward {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, None);
                assert_eq!(name, "demo.status");
                assert_eq!(arguments, serde_json::json!({}));
            }
            Bridge::Reject(response) => panic!("rejected: {:?}", response.error),
        }
    }

    #[test]
    fn malformed_or_unsupported_requests_are_rejected_with_json_rpc_errors() {
        let cases: &[(&[u8], i32)] = &[
            (b"{", -32700),
            (br#"{"jsonrpc":"1.0","id":1,"method":"tools/call"}"#, -32600),
            (br#"{"jsonrpc":"2.0","id":2,"method":"svcd.shutdown"}"#, -32601),
            (
                br#"{"jsonrpc":"2.0","id":5,"method":"registry.call","params":{"name":"status"}}"#,
                -32601,
            ),
            (br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}"#, -32602),
            (
                br#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"status","arguments":[]}}"#,
                -32602,
            ),
        ];
        for (body, code) in cases {
            match bridge_request("demo", body) {
                Bridge::Reject(response) => {
                    assert_eq!(
                        response.error.as_ref().map(|error| error.code),
                        Some(*code),
                        "{}",
                        String::from_utf8_lossy(body)
                    );
                }
                Bridge::Forward { .. } => panic!("accepted: {}", String::from_utf8_lossy(body)),
            }
        }
    }

    #[test]
    fn injects_base_after_the_doctype_so_quirks_mode_is_not_triggered() {
        let html = b"<!DOCTYPE html>\n<html><head><title>x</title></head><body></body></html>";
        let out = String::from_utf8(inject_base(html, "demo")).expect("utf8");
        let prefix = "<!DOCTYPE html>\n<base href=\"/plugin-ui/demo/\">\n";
        assert!(out.starts_with(prefix), "{out}");
        assert!(
            out.ends_with("<html><head><title>x</title></head><body></body></html>"),
            "{out}"
        );
    }

    #[test]
    fn injects_base_at_the_start_when_there_is_no_doctype() {
        let out =
            String::from_utf8(inject_base(b"<html><body>hi</body></html>", "demo")).expect("utf8");
        assert!(
            out.starts_with("<base href=\"/plugin-ui/demo/\">\n<html>"),
            "{out}"
        );
    }

    #[test]
    fn a_doctype_lookalike_in_content_is_not_mistaken_for_the_doctype() {
        let out = String::from_utf8(inject_base(
            b"<html><!-- <!doctype html> --></html>",
            "demo",
        ))
        .expect("utf8");
        assert!(out.starts_with("<base "), "{out}");
    }

    #[test]
    fn if_none_match_accepts_the_same_etag_and_weak_forms() {
        let tag = etag(UNIX_EPOCH, 7);
        assert!(not_modified(Some(&tag), &tag));
        assert!(not_modified(Some(&format!("W/{}", tag)), &tag));
        assert!(not_modified(Some(&format!("\"other\", {}", tag)), &tag));
        assert!(not_modified(Some("*"), &tag));
        assert!(!not_modified(Some("\"other\""), &tag));
        assert!(!not_modified(None, &tag));
    }

    #[test]
    fn empty_or_root_paths_serve_the_entry() {
        assert_eq!(
            relative_path("", "index.html").as_deref(),
            Some("index.html")
        );
        assert_eq!(
            relative_path("/", "index.html").as_deref(),
            Some("index.html")
        );
        assert_eq!(
            relative_path("/", "app/index.html").as_deref(),
            Some("app/index.html")
        );
    }

    #[test]
    fn relative_asset_paths_are_kept() {
        assert_eq!(
            relative_path("/app.js", "index.html").as_deref(),
            Some("app.js")
        );
        assert_eq!(
            relative_path("/assets/app.js", "index.html").as_deref(),
            Some("assets/app.js")
        );
    }

    #[test]
    fn a_trailing_slash_serves_that_directory_index() {
        assert_eq!(
            relative_path("/assets/", "index.html").as_deref(),
            Some("assets/index.html")
        );
    }

    #[test]
    fn parent_segments_and_encoded_traversal_are_rejected() {
        for path in [
            "/../secret",
            "/a/../secret",
            "/%2e%2e/secret",
            "/%2E%2E%2Fsecret",
            "/..%5csecret",
            "/a//b",
        ] {
            assert_eq!(relative_path(path, "index.html"), None, "{path}");
        }
    }

    #[test]
    fn absolute_and_backslash_paths_are_rejected() {
        for path in ["//etc/passwd", "/a\\b", "/a%00b"] {
            assert_eq!(relative_path(path, "index.html"), None, "{path}");
        }
    }
}
