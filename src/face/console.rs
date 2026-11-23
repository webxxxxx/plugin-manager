//! The console's embedded assets.
//!
//! `build.rs` gzips `src/assets/static/` into `OUT_DIR/console/`, and this
//! table is the only runtime view of it. Serving the bytes from the binary
//! keeps the console immutable: nothing writable under the data directory can
//! inject a script into the page that holds an administrator's session.
//!
//! Both assets are stored gzip-encoded and every response advertises
//! `Content-Encoding: gzip`; the page is `no-store`, the icon may be cached.

use crate::server::http::Response;

use super::webui;

/// One embedded console file and the URL that serves it.
pub(crate) struct Asset {
    /// The URL path. `/` is served from the same bytes as `/index.html`.
    pub(crate) path: &'static str,
    /// The response `Content-Type`.
    pub(crate) mime: &'static str,
    /// `Cache-Control` for this asset: the page must always be revalidated,
    /// the icon may be cached for a day.
    cache: &'static str,
    /// The build-time gzip stream.
    pub(crate) gzip: &'static [u8],
}

pub(crate) const ASSETS: &[Asset] = &[
    Asset {
        path: "/index.html",
        mime: "text/html; charset=utf-8",
        cache: "no-store",
        gzip: include_bytes!(concat!(env!("OUT_DIR"), "/console/index.html.gz")),
    },
    Asset {
        path: "/favicon.svg",
        mime: "image/svg+xml",
        cache: "public, max-age=86400",
        gzip: include_bytes!(concat!(env!("OUT_DIR"), "/console/favicon.svg.gz")),
    },
];

/// Resolve a URL path to its embedded asset. `/` shares `index.html`.
pub(crate) fn find(url_path: &str) -> Option<&'static Asset> {
    let path = if url_path == "/" {
        "/index.html"
    } else {
        url_path
    };
    ASSETS.iter().find(|asset| asset.path == path)
}

/// Build the response for one asset, including the conditional GET.
pub(crate) fn response(asset: &Asset, if_none_match: Option<&str>) -> Response {
    let etag = format!("\"{}\"", crate::manage::hex_sha256(asset.gzip));
    let (status, body): (u16, &'static [u8]) = if webui::not_modified(if_none_match, &etag) {
        (304, &[])
    } else {
        (200, asset.gzip)
    };
    Response::new(status, asset.mime, body)
        .with_header("Content-Encoding", "gzip")
        .with_header("ETag", etag)
        .with_header("Cache-Control", asset.cache)
        .with_header("X-Content-Type-Options", "nosniff")
}
