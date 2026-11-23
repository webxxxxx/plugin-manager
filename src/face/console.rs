//! The console's embedded assets.
//!
//! `build.rs` walks `src/assets/static/`, gzips the formats the media table
//! marks compressible, copies the rest, and generates `console_manifest.rs`;
//! this module is the only runtime view of that tree. `/` serves `index.html`
//! and refers to every other file through `/assets/`.
//!
//! Serving the bytes from the binary keeps the console immutable: nothing
//! writable under the data directory can inject a script into the page that
//! holds an administrator's session.

use std::sync::OnceLock;

use crate::server::http::{media, model as http, Response};

use super::webui;

#[cfg(test)]
#[path = "console_tree.rs"]
mod console_tree;

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
            .map(|(path, encoding, body)| {
                let policy = media::for_path(path);
                Asset {
                    path,
                    mime: policy.mime,
                    cache: policy.cache,
                    encoding: *encoding,
                    body,
                }
            })
            .collect()
    })
}

/// Resolve a URL path to its embedded asset.
///
/// `/` serves `index.html`; every other console URL must carry the `/assets/`
/// prefix, and the page has no second URL there (its links would resolve from
/// the wrong directory). Query strings select a representation, not an asset.
/// Percent escapes are decoded before the exact table lookup, so a name with a
/// space works while `..` can only miss.
pub(crate) fn find(url_path: &str) -> Option<&'static Asset> {
    let url_path = url_path.split_once('?').map_or(url_path, |(path, _)| path);
    if url_path == "/" {
        return assets().iter().find(|asset| asset.path == "index.html");
    }
    let relative = url_path.strip_prefix("/assets/")?;
    let relative = http::percent_decode(relative, false);
    if relative == "index.html" {
        return None;
    }
    assets().iter().find(|asset| asset.path == relative)
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
    fn embedded_manifest_matches_the_media_table() {
        for asset in assets() {
            let policy = media::for_path(asset.path);
            assert_eq!(asset.mime, policy.mime, "{}", asset.path);
            assert_eq!(asset.cache, policy.cache, "{}", asset.path);
            assert_eq!(
                asset.encoding == Encoding::Gzip,
                policy.compressible,
                "the build and the runtime disagree on gzip for {}",
                asset.path
            );
        }
    }

    #[test]
    fn every_file_in_the_static_tree_is_embedded() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/assets/static");
        let mut collected = Vec::new();
        console_tree::collect(&root, "", &mut collected);
        let mut expected: Vec<String> = collected.into_iter().map(|(path, _)| path).collect();
        expected.sort();

        let mut embedded: Vec<String> = assets()
            .iter()
            .map(|asset| asset.path.to_string())
            .collect();
        embedded.sort();
        assert_eq!(
            embedded, expected,
            "build.rs must embed every file under src/assets/static"
        );
    }

    #[test]
    fn find_decodes_escapes_ignores_the_query_and_keeps_one_page_url() {
        assert!(find("/").is_some());
        assert!(
            find("/?next=%2Fstatic%2Findex.html").is_some(),
            "the login redirect lands on /?next=…"
        );
        assert!(find("/assets/favicon.svg?v=1").is_some());
        assert!(
            find("/assets/%66avicon.svg").is_some(),
            "percent escapes decode before the lookup"
        );
        assert!(find("/assets/../Cargo.toml").is_none());
        assert!(find("/assets/missing.png").is_none());
        assert!(
            find("/index.html").is_none(),
            "the page has no /index.html URL"
        );
        assert!(
            find("/assets/index.html").is_none(),
            "the page has one URL only"
        );
        assert!(
            find("/assets/%69ndex.html").is_none(),
            "the escaped page name is not a second URL"
        );
    }
}
