//! One table for static-asset media: MIME, compression and cache policy.
//!
//! `build.rs` reads `compressible` to decide whether a console file becomes a
//! gzip stream, `face::console` reads the whole row, and `content_type` reads
//! `mime`. Keeping the rows together means one extension cannot be a text asset
//! for the build and `application/octet-stream` on the wire.

/// How one file extension is described.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Media {
    /// The response `Content-Type`.
    pub(crate) mime: &'static str,
    /// Whether the format is worth gzipping. Images, fonts and archives are
    /// already compressed and are embedded as-is.
    pub(crate) compressible: bool,
    /// `Cache-Control` for an embedded console asset.
    pub(crate) cache: &'static str,
}

/// The page never sticks; code and data revalidate with the ETag; images and
/// fonts never change without a new binary, so they may sit for a day.
const NO_STORE: &str = "no-store";
const REVALIDATE: &str = "no-cache";
const CACHE_DAY: &str = "public, max-age=86400";

/// Describe the file at `path` by its extension (ASCII case-insensitive).
pub(crate) fn for_path(path: &str) -> Media {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    for_extension(&extension)
}

fn for_extension(extension: &str) -> Media {
    let (mime, compressible, cache) = match extension {
        "html" | "htm" => ("text/html; charset=utf-8", true, NO_STORE),
        "css" => ("text/css", true, REVALIDATE),
        "js" | "mjs" => ("text/javascript", true, REVALIDATE),
        "json" | "map" => ("application/json", true, REVALIDATE),
        "svg" => ("image/svg+xml", true, REVALIDATE),
        "xml" => ("application/xml", true, REVALIDATE),
        "txt" | "md" => ("text/plain; charset=utf-8", true, REVALIDATE),
        "wasm" => ("application/wasm", true, REVALIDATE),
        "png" => ("image/png", false, CACHE_DAY),
        "jpg" | "jpeg" => ("image/jpeg", false, CACHE_DAY),
        "gif" => ("image/gif", false, CACHE_DAY),
        "webp" => ("image/webp", false, CACHE_DAY),
        "avif" => ("image/avif", false, CACHE_DAY),
        "ico" => ("image/x-icon", false, CACHE_DAY),
        "woff" => ("font/woff", false, CACHE_DAY),
        "woff2" => ("font/woff2", false, CACHE_DAY),
        "ttf" => ("font/ttf", false, CACHE_DAY),
        "otf" => ("font/otf", false, CACHE_DAY),
        "pdf" => ("application/pdf", false, REVALIDATE),
        _ => ("application/octet-stream", false, REVALIDATE),
    };
    Media {
        mime,
        compressible,
        cache,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_picks_mime_compression_and_cache() {
        for (path, mime, compressible, cache) in [
            ("index.html", "text/html; charset=utf-8", true, NO_STORE),
            ("INDEX.HTM", "text/html; charset=utf-8", true, NO_STORE),
            ("app.css", "text/css", true, REVALIDATE),
            ("app.js", "text/javascript", true, REVALIDATE),
            ("app.mjs", "text/javascript", true, REVALIDATE),
            ("data.json", "application/json", true, REVALIDATE),
            ("app.js.map", "application/json", true, REVALIDATE),
            ("logo.svg", "image/svg+xml", true, REVALIDATE),
            ("feed.xml", "application/xml", true, REVALIDATE),
            ("readme.txt", "text/plain; charset=utf-8", true, REVALIDATE),
            ("readme.md", "text/plain; charset=utf-8", true, REVALIDATE),
            ("app.wasm", "application/wasm", true, REVALIDATE),
            ("icon.png", "image/png", false, CACHE_DAY),
            ("photo.JPEG", "image/jpeg", false, CACHE_DAY),
            ("anim.gif", "image/gif", false, CACHE_DAY),
            ("image.webp", "image/webp", false, CACHE_DAY),
            ("photo.avif", "image/avif", false, CACHE_DAY),
            ("favicon.ico", "image/x-icon", false, CACHE_DAY),
            ("font.woff", "font/woff", false, CACHE_DAY),
            ("font.woff2", "font/woff2", false, CACHE_DAY),
            ("font.ttf", "font/ttf", false, CACHE_DAY),
            ("font.otf", "font/otf", false, CACHE_DAY),
            ("doc.pdf", "application/pdf", false, REVALIDATE),
            ("payload.bin", "application/octet-stream", false, REVALIDATE),
            (
                "no_extension",
                "application/octet-stream",
                false,
                REVALIDATE,
            ),
        ] {
            let media = for_path(path);
            assert_eq!(media.mime, mime, "{path}");
            assert_eq!(media.compressible, compressible, "{path}");
            assert_eq!(media.cache, cache, "{path}");
        }
    }
}
