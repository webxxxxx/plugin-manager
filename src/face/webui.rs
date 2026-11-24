//! Plugin Web UI: safe path resolution, ETag/304, and HTML base injection.
//!
//! svcd serves a plugin's `payload/web/` tree under `/plugin-ui/{id}/`. The
//! plugin never starts its own HTTP listener; `/plugin-ui/{id}/rpc` forwards a
//! JSON-RPC request to the plugin's stdio channel (see `bridge_request`), and
//! `uichannel` mirrors the plugin's notifications to SSE/WebSocket clients.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::wire::message::rpc;

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
    crate::kernel::layout::check_path(path).ok()?;
    if directory {
        Some(format!("{}/index.html", path))
    } else {
        Some(path.to_string())
    }
}

/// Resolve one URL sub-path to a file under the plugin's web root, including
/// the SPA fallback.
///
/// This is the only production place that combines the URL-to-relative
/// conversion, the rooted file lookup and the HTML-navigation fallback. A
/// missing script stays `None`; only a navigation that accepts HTML falls
/// back to the manifest entry.
pub(crate) fn resolve_asset(
    assets: &crate::manage::WebAssets,
    sub: &str,
    headers: &[(String, String)],
) -> Option<(PathBuf, std::fs::Metadata)> {
    let relative = relative_path(sub, &assets.entry)?;
    if let Some(found) = web_file(&assets.root, &relative) {
        return Some(found);
    }
    if wants_html(headers) {
        web_file(&assets.root, &assets.entry)
    } else {
        None
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

/// The conditional-request projection for one static asset.
///
/// The two faces that serve files from disk — the fallback's `serve_web_file`
/// and the plugin UI's `PluginUiStream::asset` — answer `If-None-Match` the
/// same way. This is the one place that turns a file's metadata into the
/// headers every response carries and decides whether the request already
/// holds that representation. The faces still own their own response shape
/// (buffered bytes vs. a streamed file) and their own 304 body.
pub(crate) struct AssetValidators {
    headers: Vec<(String, String)>,
    matches_if_none_match: bool,
}

impl AssetValidators {
    /// The headers both the 200 and the 304 response carry.
    pub(crate) fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Whether the request's `If-None-Match` selected this representation, so
    /// the face answers 304 with no body. Named after the header test rather
    /// than the 304: the value is the request's match, not a statement about
    /// the asset.
    pub(crate) fn matches_if_none_match(&self) -> bool {
        self.matches_if_none_match
    }
}

/// Project an asset's metadata and the request's `If-None-Match` onto its
/// validators. The modification time falls back to the Unix epoch when the
/// filesystem cannot report one, so a stat-less entry still gets a stable tag.
pub(crate) fn asset_validators(
    metadata: &std::fs::Metadata,
    if_none_match: Option<&str>,
) -> AssetValidators {
    let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
    let etag = etag(modified, metadata.len());
    AssetValidators {
        matches_if_none_match: not_modified(if_none_match, &etag),
        headers: web_headers(&etag),
    }
}

/// Insert `<base href="{base}">` so a plugin's relative assets and
/// `fetch('rpc')` resolve under its own (token-bearing) prefix.
///
/// The tag goes right after a leading doctype (inserting before it would drop
/// the document into quirks mode); without one it goes at the very start. A
/// doctype-lookalike in later content is not considered.
pub fn inject_base(html: &[u8], base: &str) -> Vec<u8> {
    let tag = format!("<base href=\"{}\">", base);
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

/// Resolve a relative path under a web root and make sure it stays inside it
/// after canonicalization (the installer rejects symlinks, but this is cheap
/// defence in depth). The path need not exist.
pub(crate) fn web_path(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        path.push(segment);
    }
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    path.starts_with(&root).then_some(path)
}

/// [`web_path`] narrowed to a regular file.
pub(crate) fn web_file(root: &Path, relative: &str) -> Option<(PathBuf, std::fs::Metadata)> {
    let path = web_path(root, relative)?;
    let metadata = std::fs::metadata(&path).ok()?;
    metadata.is_file().then_some((path, metadata))
}

/// [`web_path`] narrowed to a directory. Used by the static site's
/// slashless-directory redirect.
pub(crate) fn web_dir(root: &Path, relative: &str) -> bool {
    web_path(root, relative)
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

/// The response headers every plugin UI asset carries.
pub(crate) fn web_headers(etag: &str) -> Vec<(String, String)> {
    vec![
        ("ETag".to_string(), etag.to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("Vary".to_string(), "Accept".to_string()),
        ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
        ("Referrer-Policy".to_string(), "no-referrer".to_string()),
    ]
}

/// Whether a browser navigation asked for HTML. Used for the SPA fallback and
/// for the login redirect on an unauthenticated plugin UI page.
pub(crate) fn wants_html(headers: &[(String, String)]) -> bool {
    crate::server::http::model::header(headers, "accept")
        .map(|value| value.to_ascii_lowercase().contains("text/html"))
        .unwrap_or(false)
}

/// Percent-encode a local path for a `next=` query value so `&`, `#` or `%`
/// in a crafted `/plugin-ui/...` path cannot add query parts or a fragment.
pub(crate) fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// Insert \`<base href="{base}">\` into the head of a file without reading the
/// whole body. Returns the bytes to write before the untouched remainder and
/// the file offset where that remainder starts.
///
/// Only the first 4 KiB are inspected for a doctype. A page whose doctype
/// starts after that much whitespace gets the tag at byte 0 instead; every
/// real page puts the doctype first.
pub(crate) fn inject_base_prefix<R: Read + Seek>(
    reader: &mut R,
    length: u64,
    base: &str,
) -> std::io::Result<(Vec<u8>, u64)> {
    const PREFIX_LIMIT: usize = 4 * 1024;
    let prefix_len = std::cmp::min(length, PREFIX_LIMIT as u64) as usize;
    let mut prefix = vec![0u8; prefix_len];
    let mut filled = 0;
    if prefix_len > 0 {
        reader.seek(SeekFrom::Start(0))?;
        while filled < prefix_len {
            match reader.read(&mut prefix[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        prefix.truncate(filled);
    }
    let at = doctype_end(&prefix).unwrap_or(0);
    let tag = format!("<base href=\"{}\">", base);
    let mut head = Vec::with_capacity(at + tag.len() + 2);
    head.extend_from_slice(&prefix[..at]);
    if at > 0 {
        head.push(b'\n');
    }
    head.extend_from_slice(tag.as_bytes());
    head.push(b'\n');
    Ok((head, at as u64))
}

fn entry_path(entry: &str) -> Option<String> {
    crate::kernel::layout::check_path(entry).ok()?;
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
    fn the_prefix_injection_matches_the_in_memory_one() {
        use std::io::Cursor;

        let pages: &[&[u8]] = &[
            b"<!doctype html>\n<html><body>x</body></html>",
            b"<html><body>no doctype</body></html>",
            b"\xEF\xBB\xBF  <!DOCTYPE HTML>\n<body>",
            b"",
        ];
        for page in pages {
            let mut reader = Cursor::new(page.to_vec());
            let (head, at) =
                inject_base_prefix(&mut reader, page.len() as u64, "/x/").expect("prefix");
            let mut streamed = head;
            streamed.extend_from_slice(&page[at as usize..]);
            assert_eq!(streamed, inject_base(page, "/x/"));
        }
    }

    #[test]
    fn etag_is_mtime_plus_size_and_quoted() {
        // Windows SystemTime has 100 ns resolution, so use a microsecond multiple.
        let modified =
            UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_micros(123_456);
        assert_eq!(etag(modified, 42), "\"1700000000.123456000-42\"");
    }

    #[test]
    fn one_projection_decides_the_304_and_the_headers_of_both_paths() {
        let root = crate::platform::test_support::temp_dir().join(format!(
            "svcd-webui-validators-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let path = root.join("app.js");
        std::fs::write(&path, b"console.log(1);").expect("write asset");
        let metadata = std::fs::metadata(&path).expect("metadata");
        let tag = etag(metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len());

        let hit = asset_validators(&metadata, Some(&format!("W/{}", tag)));
        assert!(
            hit.matches_if_none_match(),
            "a weak form of the current tag is a 304"
        );
        // Pin the whole set literally: dropping a cache header here must fail.
        assert_eq!(
            hit.headers(),
            [
                ("ETag".to_string(), tag.clone()),
                ("Cache-Control".to_string(), "no-cache".to_string()),
                ("Vary".to_string(), "Accept".to_string()),
                ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
                ("Referrer-Policy".to_string(), "no-referrer".to_string()),
            ]
        );

        let miss = asset_validators(&metadata, Some("\"other\""));
        assert!(
            !miss.matches_if_none_match(),
            "a different tag keeps the 200 path"
        );
        assert_eq!(
            miss.headers(),
            hit.headers(),
            "the 200 path carries the same header set as the 304"
        );

        let _ = std::fs::remove_dir_all(&root);
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
        let out = String::from_utf8(inject_base(html, "/plugin-ui/demo/tok/")).expect("utf8");
        let prefix = "<!DOCTYPE html>\n<base href=\"/plugin-ui/demo/tok/\">\n";
        assert!(out.starts_with(prefix), "{out}");
        assert!(
            out.ends_with("<html><head><title>x</title></head><body></body></html>"),
            "{out}"
        );
    }

    #[test]
    fn injects_base_at_the_start_when_there_is_no_doctype() {
        let out = String::from_utf8(inject_base(
            b"<html><body>hi</body></html>",
            "/plugin-ui/demo/tok/",
        ))
        .expect("utf8");
        assert!(
            out.starts_with("<base href=\"/plugin-ui/demo/tok/\">\n<html>"),
            "{out}"
        );
    }

    #[test]
    fn a_doctype_lookalike_in_content_is_not_mistaken_for_the_doctype() {
        let out = String::from_utf8(inject_base(
            b"<html><!-- <!doctype html> --></html>",
            "/plugin-ui/demo/tok/",
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
