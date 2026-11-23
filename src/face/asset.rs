//! A sans-IO response for one file on disk.
//!
//! The plugin UI and the configured static site both serve files from a
//! canonicalized root. This module owns the bytes: conditional GET, the HTML
//! `<base>` injection the plugin UI needs, and the zero-copy file send. A face
//! decides which file a URL maps to; this type only sends it.
//!
//! Static assets are one-shot streams: they never take a `max_streams` slot,
//! and the connection's server loop never reads a whole page into memory.

use std::fs;
use std::path::Path;

use crate::face::webui;
use crate::server::http::model as http;
use crate::server::http::{
    CloseReason, FileSend, OutBuffer, StreamAction, StreamClass, StreamEvent, StreamHandler,
    StreamStep,
};

/// One file or one already-rendered response as a stream handler.
pub(crate) struct AssetStream {
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

impl AssetStream {
    pub(crate) fn response(bytes: Vec<u8>) -> AssetStream {
        AssetStream {
            body: Body::Bytes { bytes, sent: 0 },
        }
    }

    pub(crate) fn error(status: u16, code: &str, message: &str) -> AssetStream {
        AssetStream::response(http::json_error(status, code, message))
    }

    /// Resolve one `/plugin-ui/{id}/{token}/...` sub-path against the plugin's
    /// web root. A missing script stays 404; only an HTML navigation falls back
    /// to the manifest entry.
    pub(crate) fn asset(
        headers: &[(String, String)],
        sub: &str,
        assets: &crate::manage::WebAssets,
        base: &str,
    ) -> AssetStream {
        let Some((path, metadata)) = webui::resolve_asset(assets, sub, headers) else {
            return AssetStream::error(404, "not_found", "not found");
        };
        AssetStream::file(headers, &path, &metadata, Some(base), false)
    }

    /// One file from the configured static-site root. The static site does not
    /// inject a `<base>` tag: relative URLs are authored against `/static/`.
    /// `head_only` still sends the real `Content-Length` but no file bytes.
    pub(crate) fn static_file(
        headers: &[(String, String)],
        path: &Path,
        metadata: &fs::Metadata,
        head_only: bool,
    ) -> AssetStream {
        AssetStream::file(headers, path, metadata, None, head_only)
    }

    fn file(
        headers: &[(String, String)],
        path: &Path,
        metadata: &fs::Metadata,
        base: Option<&str>,
        head_only: bool,
    ) -> AssetStream {
        let mime = http::content_type(&path.to_string_lossy());
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) => {
                return AssetStream::error(500, "internal", &format!("open asset: {}", error))
            }
        };
        let validators = webui::asset_validators(metadata, http::header(headers, "if-none-match"));
        if validators.matches_if_none_match() {
            return AssetStream::response(http::response_with_headers(
                304,
                mime,
                validators.headers(),
                b"",
            ));
        }
        let length = metadata.len();
        if let Some(base) = base {
            if mime.starts_with("text/html") {
                // Insert the base tag from a bounded prefix, then send the rest
                // of the page from disk. The event loop never reads a whole page.
                let (injected, offset) = match webui::inject_base_prefix(&mut file, length, base) {
                    Ok(result) => result,
                    Err(error) => {
                        return AssetStream::error(
                            500,
                            "internal",
                            &format!("read UI asset: {}", error),
                        )
                    }
                };
                // `injected` already contains the file's first `offset` bytes,
                // so the remainder is `length - offset`; do not count them twice.
                let body_len = length + injected.len() as u64 - offset;
                let mut head = http::response_head(200, mime, validators.headers(), body_len);
                head.extend_from_slice(&injected);
                if offset >= length {
                    return AssetStream::response(head);
                }
                return AssetStream {
                    body: Body::File {
                        head,
                        head_sent: 0,
                        file,
                        offset,
                        length: length - offset,
                    },
                };
            }
        }
        let head = http::response_head(200, mime, validators.headers(), length);
        if length == 0 || head_only {
            // No file bytes to hand over; the head is the whole response.
            return AssetStream::response(head);
        }
        AssetStream {
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

impl StreamHandler for AssetStream {
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
