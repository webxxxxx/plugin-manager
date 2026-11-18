//! `/plugin-ui` face: a plugin's static web assets as a sans-IO stream.
//!
//! The server owns the socket and the output buffer. This face only answers
//! "which file is this URL" and "what bytes come next"; a non-HTML asset is
//! handed back as a server-side file send so the zero-copy path survives.
//! A browser loading one page therefore costs one connection, not one thread
//! per asset, and a one-shot asset never consumes `max_streams`.

use crate::face::plugin_streams::{self, StreamContext};
use crate::face::webui;
use crate::manage::package;
use crate::manage::plugin;
use crate::platform::config::Config;
use crate::server::auth::lan::Lan;
use crate::server::http::model as http;
use crate::server::http::{
    dispatch_inline, BodyPolicy, CloseReason, Completion, Exchange, Face, FileSend, OutBuffer,
    Request, Route, StreamAction, StreamClass, StreamEvent, StreamHandler, StreamStep,
};
use std::fs;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// One parsed `/plugin-ui/{id}/{token}/...` path.
pub(crate) struct UiPath<'a> {
    /// The plugin id segment (still unvalidated).
    pub(crate) id: &'a str,
    /// The per-plugin UI token.
    pub(crate) token: &'a str,
    /// Everything after the token; empty for the plugin root.
    pub(crate) sub: &'a str,
    /// True for `/plugin-ui/{id}` or `/plugin-ui/{id}/{token}` without the
    /// trailing slash.
    pub(crate) slashless: bool,
}

pub(crate) fn ui_path(path: &str) -> Option<UiPath<'_>> {
    let rest = path.strip_prefix("/plugin-ui/")?;
    let (id, after, root_slash) = match rest.split_once('/') {
        Some((id, after)) => (id, after, true),
        None => (rest, "", false),
    };
    if after.is_empty() && root_slash {
        return Some(UiPath {
            id,
            token: "",
            sub: "",
            slashless: false,
        });
    }
    let (token, sub, slashless) = match after.split_once('/') {
        Some((token, sub)) => (token, sub, false),
        None => (after, "", true),
    };
    Some(UiPath {
        id,
        token,
        sub,
        slashless,
    })
}

fn path_only(path: &str) -> &str {
    path.split_once('?').map(|(path, _)| path).unwrap_or(path)
}

/// SSE and WebSocket clients are not browser navigations: an unauthenticated
/// request gets a 401, never the console login redirect.
fn is_realtime(path: &str) -> bool {
    ui_path(path)
        .map(|ui| matches!(ui.sub, "events" | "ws" | "raw"))
        .unwrap_or(false)
}

/// The face for everything under `/plugin-ui`: static assets, SSE, the page
/// WebSocket and the protocol-3 raw WebSocket.
pub struct PluginUiFace {
    lan: Arc<Mutex<Lan>>,
    /// The realtime routes need the config path for the control-channel bridge
    /// and the raw hub.
    streams: Arc<StreamContext>,
}

impl PluginUiFace {
    pub fn with_streams(
        lan: Arc<Mutex<Lan>>,
        config: Arc<Config>,
        raw_hub: Option<Arc<crate::wire::contract::rawhub::RawHub>>,
    ) -> PluginUiFace {
        PluginUiFace {
            lan,
            streams: Arc::new(StreamContext {
                paths: config.paths.clone(),
                raw_hub,
                limits: config.limits.clone(),
            }),
        }
    }

    /// The route decision, usable before the server has resolved a principal.
    /// Every realtime substream and every static asset is a `Streamed` route:
    /// the body (if any) is consumed through stream events, and the success
    /// result is a stream handler. `/rpc` stays on the buffered router.
    pub fn route_for(method: &str, path: &str) -> Option<Route> {
        if method != "GET" {
            return None;
        }
        let ui = ui_path(path_only(path))?;
        if ui.slashless || ui.sub == "rpc" {
            return None;
        }
        Some(Route::inline(BodyPolicy::Streamed))
    }

    /// Resolve the principal and dispatch one plugin UI request. `Err` is the
    /// response to send before the face runs: the login redirect for a browser
    /// navigation, a 401 for an API client, or a dispatch error.
    ///
    /// Both engines use this helper so endpoint policy and the wire bytes have
    /// one implementation; the caller owns the socket.
    pub(crate) fn exchange(
        head: &http::Head,
        peer: IpAddr,
        lan: &Arc<Mutex<Lan>>,
        config: Arc<Config>,
        raw_hub: Option<Arc<crate::wire::contract::rawhub::RawHub>>,
    ) -> Result<Exchange, Vec<u8>> {
        let Some(route) = PluginUiFace::route_for(&head.method, &head.path) else {
            return Err(http::json_error(404, "not_found", "not a plugin UI path"));
        };
        let principal = match lan.lock() {
            Ok(mut state) => state.authenticate(&head.headers),
            Err(_) => Err(()),
        };
        let principal = match principal {
            Ok(principal) => principal,
            Err(_) => {
                if is_realtime(path_only(&head.path)) {
                    return Err(http::json_error(
                        401,
                        "unauthorized",
                        "authentication required",
                    ));
                }
                return Err(unauthenticated(&head.headers, path_only(&head.path)));
            }
        };
        let request = Request {
            method: head.method.clone(),
            path: head.path.clone(),
            headers: head.headers.clone(),
            peer,
            principal,
            body: Vec::new(),
            deadline: None,
            cancel: None,
        };
        let face = PluginUiFace::with_streams(lan.clone(), config, raw_hub);
        dispatch_inline(&face, &route, request)
            .map_err(|error| http::json_error(500, "internal", &error.message()))
    }

    /// Endpoint policy: a valid plugin id with a web UI, and a token minted for
    /// that same plugin. Failures are response streams too, because a
    /// `Streamed` route may not complete with a memory response.
    fn open(&self, request: &Request) -> Box<dyn StreamHandler> {
        let path = request.path_only();
        let Some(ui) = ui_path(path) else {
            return Box::new(PluginUiStream::error(404, "not_found", "unknown plugin UI"));
        };
        match ui.sub {
            "events" => {
                return plugin_streams::open_sse(&ui, request, &self.lan, &self.streams);
            }
            "ws" => return plugin_streams::open_ws(&ui, request, &self.lan, &self.streams),
            "raw" => return plugin_streams::open_raw(&ui, request, &self.lan, &self.streams),
            _ => {}
        }
        if !package::valid_name(ui.id) {
            return Box::new(PluginUiStream::error(404, "not_found", "unknown plugin UI"));
        }
        let Some(assets) = plugin::web_assets(&self.streams.paths, ui.id) else {
            return Box::new(PluginUiStream::error(
                404,
                "not_found",
                &format!("plugin {} has no web UI", ui.id),
            ));
        };
        if !crate::server::auth::admission::plugin_token_valid(&self.lan, ui.token, ui.id) {
            return Box::new(PluginUiStream::error(
                401,
                "unauthorized",
                "invalid or expired plugin token",
            ));
        }
        let base = format!("/plugin-ui/{}/{}/", ui.id, ui.token);
        Box::new(PluginUiStream::asset(
            &request.headers,
            ui.sub,
            &assets,
            &base,
        ))
    }
}

impl Face for PluginUiFace {
    fn route(&self, method: &str, path: &str) -> Option<Route> {
        PluginUiFace::route_for(method, path)
    }

    fn handle(&self, request: Request, done: Completion) {
        done.complete(Exchange::Stream(self.open(&request)));
    }
}

/// The response an unauthenticated plugin UI request gets. A browser
/// navigation goes to the console login; API clients keep the 401.
pub(crate) fn unauthenticated(headers: &[(String, String)], path: &str) -> Vec<u8> {
    if webui::wants_html(headers) {
        http::response_with_headers(
            303,
            "text/plain; charset=utf-8",
            &[(
                "Location".to_string(),
                format!("/?next={}", webui::encode_query_value(path)),
            )],
            b"",
        )
    } else {
        http::json_error(401, "unauthorized", "authentication required")
    }
}

/// A plugin UI asset response as a sans-IO state machine.
pub(crate) struct PluginUiStream {
    body: Body,
}

enum Body {
    Bytes {
        bytes: Vec<u8>,
        sent: usize,
    },
    File {
        head: Vec<u8>,
        head_sent: usize,
        file: fs::File,
        offset: u64,
        length: u64,
    },
    Done,
}

impl PluginUiStream {
    pub(crate) fn response(bytes: Vec<u8>) -> PluginUiStream {
        PluginUiStream {
            body: Body::Bytes { bytes, sent: 0 },
        }
    }

    fn error(status: u16, code: &str, message: &str) -> PluginUiStream {
        PluginUiStream::response(http::json_error(status, code, message))
    }

    /// Resolve one `/plugin-ui/{id}/{token}/...` sub-path against the plugin's
    /// web root. This is the decision half; the events drive the bytes.
    pub(crate) fn asset(
        headers: &[(String, String)],
        sub: &str,
        assets: &plugin::WebAssets,
        base: &str,
    ) -> PluginUiStream {
        let Some(relative) = webui::relative_path(sub, &assets.entry) else {
            return PluginUiStream::error(404, "not_found", "not found");
        };
        // SPA fallback: only an HTML navigation falls back to the entry. A
        // missing script must stay a 404 so the browser reports it instead of
        // running HTML.
        let file = webui::web_file(&assets.root, &relative).or_else(|| {
            if webui::wants_html(headers) {
                webui::web_file(&assets.root, &assets.entry)
            } else {
                None
            }
        });
        let Some((path, _)) = file else {
            return PluginUiStream::error(404, "not_found", "not found");
        };
        let mime = http::content_type(&path.to_string_lossy());
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                return PluginUiStream::error(500, "internal", &format!("open UI asset: {}", error))
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                return PluginUiStream::error(500, "internal", &format!("stat UI asset: {}", error))
            }
        };
        let validators = webui::asset_validators(&metadata, http::header(headers, "if-none-match"));
        if validators.matches_if_none_match() {
            return PluginUiStream::response(http::response_with_headers(
                304,
                mime,
                validators.headers(),
                b"",
            ));
        }
        let length = metadata.len();
        if mime.starts_with("text/html") {
            // Insert the base tag from a bounded prefix, then send the rest of
            // the page from disk. The event loop never reads a whole page.
            let (injected, offset) = match webui::inject_base_prefix(&mut file, length, base) {
                Ok(result) => result,
                Err(error) => {
                    return PluginUiStream::error(
                        500,
                        "internal",
                        &format!("read UI asset: {}", error),
                    )
                }
            };
            // `injected` already contains the file's first `offset` bytes, so
            // the remainder is `length - offset`; do not count them twice.
            let body_len = length + injected.len() as u64 - offset;
            let mut head = http::response_head(200, mime, validators.headers(), body_len);
            head.extend_from_slice(&injected);
            if offset >= length {
                return PluginUiStream::response(head);
            }
            return PluginUiStream {
                body: Body::File {
                    head,
                    head_sent: 0,
                    file,
                    offset,
                    length: length - offset,
                },
            };
        }
        let head = http::response_head(200, mime, validators.headers(), length);
        if length == 0 {
            // No file bytes to hand over; the head is the whole response.
            return PluginUiStream::response(head);
        }
        PluginUiStream {
            body: Body::File {
                head,
                head_sent: 0,
                file,
                offset: 0,
                length,
            },
        }
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        // Take the body out so the file handle can move into a `FileSend`; put
        // it back when a bounded write leaves work for the next `Writable`.
        let body = std::mem::replace(&mut self.body, Body::Done);
        match body {
            Body::Done => StreamStep::wait(StreamAction::Close(CloseReason::Completed)),
            Body::Bytes { bytes, mut sent } => {
                let remaining = bytes.len().saturating_sub(sent);
                let take = remaining.min(out.remaining());
                let complete = out.write(&bytes[sent..sent + take]);
                debug_assert!(complete, "the chunk was clamped to the free capacity");
                sent += take;
                if sent == bytes.len() {
                    StreamStep::wait(StreamAction::Close(CloseReason::Completed))
                } else {
                    self.body = Body::Bytes { bytes, sent };
                    StreamStep::wait(StreamAction::NeedWrite)
                }
            }
            Body::File {
                head,
                mut head_sent,
                file,
                offset,
                length,
            } => {
                if head_sent < head.len() {
                    let take = (head.len() - head_sent).min(out.remaining());
                    let complete = out.write(&head[head_sent..head_sent + take]);
                    debug_assert!(complete, "the chunk was clamped to the free capacity");
                    head_sent += take;
                    if head_sent < head.len() {
                        self.body = Body::File {
                            head,
                            head_sent,
                            file,
                            offset,
                            length,
                        };
                        return StreamStep::wait(StreamAction::NeedWrite);
                    }
                }
                StreamStep::wait(StreamAction::SendFile(FileSend {
                    file,
                    offset,
                    length,
                }))
            }
        }
    }
}

impl StreamHandler for PluginUiStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Tick(_) => StreamStep::wait(StreamAction::Close(CloseReason::TimedOut)),
            StreamEvent::Closed(reason) => StreamStep::wait(StreamAction::Close(reason)),
            // A static response has no request body and submits no blocking
            // job; both events are legal and change nothing.
            StreamEvent::Data(_) | StreamEvent::BlockingDone(_) => {
                StreamStep::wait(StreamAction::Continue)
            }
        }
    }

    fn stream_class(&self) -> StreamClass {
        StreamClass::OneShot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::http::engine::HttpRequest;
    use crate::server::http::BodyPolicy;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    const PAGE: &[u8] =
        b"<!doctype html>\n<html><head><title>demo</title></head><body>plugin page</body></html>";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let name = format!("svcd-plugin-ui-{}-{}-{}", std::process::id(), id, nanos);
            // The integration suite already uses `.scratch/tmp`; a read-only
            // checkout still falls back to the OS temp directory.
            let mut bases = vec![
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(".scratch")
                    .join("tmp"),
                std::env::temp_dir(),
            ];
            while let Some(base) = bases.pop() {
                if fs::create_dir_all(&base).is_err() {
                    continue;
                }
                let path = base.join(&name);
                if fs::create_dir_all(&path).is_ok() {
                    return TempDir(path);
                }
            }
            panic!("cannot create a temporary web root for the plugin UI test");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn assets(dir: &TempDir) -> plugin::WebAssets {
        plugin::WebAssets {
            root: dir.0.clone(),
            entry: "index.html".to_string(),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn drive(mut handler: PluginUiStream, limit: usize) -> (Vec<u8>, CloseReason) {
        use std::io::{Read, Seek, SeekFrom};

        let mut all = Vec::new();
        let mut out = Vec::new();
        for _ in 0..10_000 {
            let step =
                handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
            assert!(
                out.len() <= limit,
                "a writable event must respect the buffer limit"
            );
            all.extend_from_slice(&out);
            out.clear();
            match step.action {
                StreamAction::Close(reason) => return (all, reason),
                StreamAction::NeedWrite | StreamAction::Continue | StreamAction::NeedRead => {}
                StreamAction::SendFile(mut send) => {
                    // The server would send this range on the socket. Read it
                    // back so the test can assert the exact response bytes.
                    send.file
                        .seek(SeekFrom::Start(send.offset))
                        .expect("seek asset");
                    let mut reader = (&mut send.file).take(send.length);
                    reader.read_to_end(&mut all).expect("read asset remainder");
                    return (all, CloseReason::Completed);
                }
                StreamAction::SubmitBlocking(_) => panic!("a static asset submits no job"),
            }
        }
        panic!("the handler never finished");
    }

    #[test]
    fn only_plugin_ui_gets_are_claimed() {
        assert!(PluginUiFace::route_for("GET", "/plugin-ui/demo/tok/").is_some());
        assert!(PluginUiFace::route_for("GET", "/plugin-ui/demo/tok/app.js?x=1").is_some());
        assert!(PluginUiFace::route_for("GET", "/plugin-ui/Demo/tok/deep/page").is_some());
        assert!(PluginUiFace::route_for("POST", "/plugin-ui/demo/tok/").is_none());
        assert!(PluginUiFace::route_for("GET", "/plugin-ui/demo").is_none());
        assert!(
            PluginUiFace::route_for("GET", "/plugin-ui/demo/tok/rpc").is_none(),
            "/rpc stays on the buffered router"
        );
        for sub in ["events", "ws", "raw"] {
            let route = PluginUiFace::route_for("GET", &format!("/plugin-ui/demo/tok/{}", sub))
                .unwrap_or_else(|| panic!("{sub} is a streamed route"));
            assert_eq!(route.body, BodyPolicy::Streamed, "{sub}");
        }
        let route = PluginUiFace::route_for("GET", "/plugin-ui/demo/tok/app.js").expect("route");
        assert_eq!(route.body, BodyPolicy::Streamed);
    }

    #[test]
    fn an_html_page_gets_the_plugin_base_and_etag() {
        let dir = TempDir::new();
        fs::write(dir.0.join("index.html"), PAGE).expect("write page");
        let handler = PluginUiStream::asset(
            &headers(&[("Accept", "text/html")]),
            "",
            &assets(&dir),
            "/plugin-ui/demo/tok/",
        );
        let (bytes, reason) = drive(handler, 1024);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(
            text.contains("Content-Type: text/html; charset=utf-8\r\n"),
            "{text}"
        );
        assert!(text.contains("Cache-Control: no-cache\r\n"), "{text}");
        assert!(text.contains("Vary: Accept\r\n"), "{text}");
        assert!(
            text.contains("<base href=\"/plugin-ui/demo/tok/\">\n"),
            "{text}"
        );
        assert!(text.ends_with("plugin page</body></html>"), "{text}");
        assert_eq!(reason, CloseReason::Completed);
    }

    #[test]
    fn an_matching_etag_returns_304_without_a_body() {
        let dir = TempDir::new();
        fs::write(dir.0.join("index.html"), PAGE).expect("write page");
        let metadata = fs::metadata(dir.0.join("index.html")).expect("metadata");
        let etag = webui::etag(metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len());
        let request_headers = headers(&[("If-None-Match", &etag)]);
        let handler =
            PluginUiStream::asset(&request_headers, "", &assets(&dir), "/plugin-ui/demo/tok/");
        let (bytes, reason) = drive(handler, 1024);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 304 Not Modified\r\n"), "{text}");
        assert!(text.contains(&format!("ETag: {}\r\n", etag)), "{text}");
        assert_eq!(reason, CloseReason::Completed);
    }

    /// One metadata plus one `If-None-Match` must give both faces the same
    /// ETag header and the same 304 decision. The streamed face is driven with
    /// the bytes the server would write; the buffered face is the fallback's
    /// own response builder, reached here through its crate-visible entry.
    #[test]
    fn both_faces_agree_on_the_conditional_request_for_one_asset() {
        let dir = TempDir::new();
        fs::write(dir.0.join("app.js"), b"console.log(1);").expect("write asset");
        let path = dir.0.join("app.js");
        let metadata = fs::metadata(&path).expect("metadata");
        let tag = webui::etag(metadata.modified().unwrap_or(UNIX_EPOCH), metadata.len());
        let base = "/plugin-ui/demo/tok/";

        for (if_none_match, status) in [
            (tag.clone(), "HTTP/1.1 304 Not Modified\r\n"),
            ("\"other\"".to_string(), "HTTP/1.1 200 OK\r\n"),
        ] {
            let request_headers = headers(&[("If-None-Match", if_none_match.as_str())]);
            let (streamed, _) = drive(
                PluginUiStream::asset(&request_headers, "app.js", &assets(&dir), base),
                4096,
            );
            let buffered = crate::face::fallback::serve_web_file(
                &HttpRequest {
                    method: "GET".into(),
                    path: "/plugin-ui/demo/tok/app.js".into(),
                    version: "HTTP/1.1".into(),
                    headers: request_headers,
                    body: Vec::new(),
                },
                &path,
                &metadata,
                base,
            );
            for (side, response) in [("streamed", streamed), ("buffered", buffered)] {
                let response = String::from_utf8_lossy(&response).to_string();
                assert!(response.starts_with(status), "{side}: {response}");
                assert!(
                    response.contains(&format!("ETag: {}\r\n", tag)),
                    "{side}: {response}"
                );
                assert!(
                    response.contains("Cache-Control: no-cache\r\n"),
                    "{side}: {response}"
                );
                if status.contains("304") {
                    assert!(
                        response.ends_with("\r\n\r\n"),
                        "{side} 304 must carry no body: {response}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_large_page_is_written_across_bounded_writable_events() {
        let dir = TempDir::new();
        let mut html = Vec::from(&b"<!doctype html>\n<body>\n"[..]);
        for line in 0..400 {
            html.extend_from_slice(
                format!("<p>line {} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx</p>\n", line).as_bytes(),
            );
        }
        fs::write(dir.0.join("index.html"), &html).expect("write page");
        let handler = PluginUiStream::asset(&[], "", &assets(&dir), "/plugin-ui/demo/tok/");
        let (all, reason) = drive(handler, 64);
        assert_eq!(reason, CloseReason::Completed);
        let text = String::from_utf8_lossy(&all);
        assert!(text.ends_with("</p>\n"), "the last chunk is not truncated");
        assert!(
            all.len() > 4096,
            "the page is larger than one writable event"
        );
        let expected = webui::inject_base(&html, "/plugin-ui/demo/tok/");
        assert!(
            text.contains(&format!("Content-Length: {}\r\n", expected.len())),
            "the injected body length is announced: expected {} in {:?}",
            expected.len(),
            &text[..text.len().min(300)]
        );
    }

    #[test]
    fn split_data_tick_and_close_events_need_no_socket() {
        let dir = TempDir::new();
        fs::write(dir.0.join("index.html"), PAGE).expect("write page");
        let mut handler = PluginUiStream::asset(
            &headers(&[("Accept", "text/html")]),
            "",
            &assets(&dir),
            "/plugin-ui/demo/tok/",
        );
        let mut out = Vec::new();
        for chunk in [
            &b"GET"[..],
            &b" /plugin-ui/demo/tok/ HTTP/1.1\r\n"[..],
            &b"Accept: text/html\r\n\r\n"[..],
        ] {
            let step =
                handler.on_event(StreamEvent::Data(chunk), &mut OutBuffer::new(&mut out, 64));
            assert!(matches!(step.action, StreamAction::Continue));
            assert!(
                out.is_empty(),
                "an asset response does not answer request bytes"
            );
        }
        let step = handler.on_event(
            StreamEvent::Tick(Instant::now()),
            &mut OutBuffer::new(&mut out, 64),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::TimedOut)
        ));
        let step = handler.on_event(
            StreamEvent::Closed(CloseReason::PeerClosed),
            &mut OutBuffer::new(&mut out, 64),
        );
        assert!(matches!(
            step.action,
            StreamAction::Close(CloseReason::PeerClosed)
        ));
    }

    #[test]
    fn a_non_html_asset_requests_a_server_side_file_send() {
        let dir = TempDir::new();
        let script = b"export const answer = 42;\n";
        fs::write(dir.0.join("app.js"), script).expect("write asset");
        let mut handler =
            PluginUiStream::asset(&[], "app.js", &assets(&dir), "/plugin-ui/demo/tok/");
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"), "{text}");
        assert!(text.contains("Content-Type: text/javascript\r\n"), "{text}");
        assert!(
            text.contains(&format!("Content-Length: {}\r\n", script.len())),
            "{text}"
        );
        let StreamAction::SendFile(send) = step.action else {
            panic!("a non-HTML asset must keep the zero-copy file path");
        };
        assert_eq!(send.offset, 0);
        assert_eq!(send.length, script.len() as u64);
        let mut content = Vec::new();
        let mut file = send.file;
        std::io::Read::read_to_end(&mut file, &mut content).expect("read the sent file");
        assert_eq!(content, script);
    }

    #[test]
    fn a_missing_asset_is_404_except_for_an_html_navigation() {
        let dir = TempDir::new();
        fs::write(dir.0.join("index.html"), PAGE).expect("write page");
        let handler =
            PluginUiStream::asset(&[], "missing.js", &assets(&dir), "/plugin-ui/demo/tok/");
        let (bytes, _) = drive(handler, 1024);
        assert!(
            String::from_utf8_lossy(&bytes).starts_with("HTTP/1.1 404 Not Found"),
            "a missing script must stay a 404"
        );
        let handler = PluginUiStream::asset(
            &headers(&[("Accept", "text/html")]),
            "settings/tunnel",
            &assets(&dir),
            "/plugin-ui/demo/tok/",
        );
        let (bytes, _) = drive(handler, 4096);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 200 OK"), "{text}");
        assert!(text.contains("plugin page"), "{text}");
    }

    #[test]
    fn a_token_error_is_still_a_stream_response() {
        let handler = PluginUiStream::error(401, "unauthorized", "invalid or expired plugin token");
        let (bytes, reason) = drive(handler, 1024);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized"), "{text}");
        assert!(text.contains("invalid or expired plugin token"), "{text}");
        assert_eq!(reason, CloseReason::Completed);
    }

    #[test]
    fn the_asset_handler_is_a_one_shot_stream() {
        let handler = PluginUiStream::response(Vec::new());
        assert_eq!(handler.stream_class(), StreamClass::OneShot);
    }
}
