//! The console's embedded assets.
//!
//! `build.rs` walks `src/assets/static/`, gzips the text formats, copies the
//! binary ones, and generates `console_manifest.rs`; this module is the only
//! runtime view of that tree. Every file is served under `/assets/`, while `/`
//! serves `index.html`.
//!
//! Serving the bytes from the binary keeps the console immutable: nothing
//! writable under the data directory can inject a script into the page that
//! holds an administrator's session.

use std::sync::OnceLock;

use crate::server::http::{model as http, Response};

use super::webui;

/// How one embedded asset is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    Gzip,
    /// Build.rs emits this for images and fonts. The current tree happens to
    /// be all text, so keep the variant alive until the first image lands.
    #[allow(dead_code)]
    Identity,
}

/// One embedded console file, addressed by its path under `/assets/`.
pub(crate) struct Asset {
    /// The path relative to `src/assets/static/`, using `/` separators.
    pub(crate) path: &'static str,
    /// The response `Content-Type`.
    pub(crate) mime: &'static str,
    /// `Cache-Control` for this asset.
    cache: &'static str,
    /// How `body` is encoded on the wire.
    pub(crate) encoding: Encoding,
    /// The bytes embedded in the binary.
    pub(crate) body: &'static [u8],
}

type GeneratedAsset = (&'static str, Encoding, &'static [u8]);

include!(concat!(env!("OUT_DIR"), "/console_manifest.rs"));

/// The generated table with MIME and cache policy derived from each path.
pub(crate) fn assets() -> &'static [Asset] {
    static ASSETS: OnceLock<Vec<Asset>> = OnceLock::new();
    ASSETS.get_or_init(|| {
        GENERATED
            .iter()
            .map(|(path, encoding, body)| Asset {
                path,
                mime: http::content_type(path),
                cache: cache_for(path),
                encoding: *encoding,
                body,
            })
            .collect()
    })
}

/// Resolve a URL path to its embedded asset.
///
/// `/` serves `index.html`; every other console URL must carry the `/assets/`
/// prefix. Percent escapes are decoded before the exact table lookup, so a
/// name with a space works while `..` can only miss.
pub(crate) fn find(url_path: &str) -> Option<&'static Asset> {
    let relative = if url_path == "/" {
        "index.html"
    } else {
        url_path.strip_prefix("/assets/")?
    };
    let relative = http::percent_decode(relative, false);
    assets().iter().find(|asset| asset.path == relative)
}

/// The page is never cached; text assets revalidate with the ETag; images and
/// fonts may sit in the browser cache for a day.
fn cache_for(path: &str) -> &'static str {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "no-store",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "ico" | "woff" | "woff2" | "ttf"
        | "otf" => "public, max-age=86400",
        _ => "no-cache",
    }
}

/// Build the response for one asset, including the conditional GET.
pub(crate) fn response(asset: &Asset, if_none_match: Option<&str>) -> Response {
    let etag = format!("\"{}\"", crate::manage::hex_sha256(asset.body));
    let (status, body): (u16, &'static [u8]) = if webui::not_modified(if_none_match, &etag) {
        (304, &[])
    } else {
        (200, asset.body)
    };
    let mut response = Response::new(status, asset.mime, body)
        .with_header("ETag", etag)
        .with_header("Cache-Control", asset.cache)
        .with_header("X-Content-Type-Options", "nosniff");
    if asset.encoding == Encoding::Gzip {
        response = response.with_header("Content-Encoding", "gzip");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_assets_are_served_without_content_encoding() {
        let asset = Asset {
            path: "logo.png",
            mime: "image/png",
            cache: "public, max-age=86400",
            encoding: Encoding::Identity,
            body: b"\x89PNG\r\n",
        };
        let response = response(&asset, None);
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "image/png");
        assert_eq!(response.body, b"\x89PNG\r\n");
        assert!(
            !response
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-encoding")),
            "an already-compressed image must not be re-encoded"
        );
        assert!(response
            .headers
            .iter()
            .any(|(name, value)| name == "Cache-Control" && value == "public, max-age=86400"));
    }

    #[test]
    fn cache_policy_follows_the_extension() {
        assert_eq!(cache_for("index.html"), "no-store");
        assert_eq!(cache_for("app.css"), "no-cache");
        assert_eq!(cache_for("app.js"), "no-cache");
        assert_eq!(cache_for("logo.svg"), "no-cache");
        assert_eq!(cache_for("logo.png"), "public, max-age=86400");
        assert_eq!(cache_for("font.woff2"), "public, max-age=86400");
    }

    #[test]
    fn find_decodes_escapes_and_refuses_paths_outside_the_table() {
        assert!(find("/assets/index.html").is_some());
        assert!(find("/assets/%69ndex.html").is_some());
        assert!(find("/assets/../Cargo.toml").is_none());
        assert!(find("/assets/missing.png").is_none());
        assert!(find("/index.html").is_none());
    }
}
