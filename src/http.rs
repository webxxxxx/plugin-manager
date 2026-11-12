//! HTTP primitives; the UI reverse proxy lands in #33.
#![allow(dead_code)]

pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub headers: Vec<(&'a str, &'a str)>,
}

pub fn parse(buf: &[u8]) -> Option<Request<'_>> {
    let text = std::str::from_utf8(buf).ok()?;
    let mut lines = text.split("\r\n");
    let first = lines.next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim(), v.trim()));
        }
    }
    Some(Request {
        method,
        path,
        headers,
    })
}

pub fn response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    response_with_headers(status, content_type, &[], body)
}

pub fn response_with_headers(
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let mut out = Vec::with_capacity(128 + body.len());
    out.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            status,
            reason,
            content_type,
            body.len()
        )
        .as_bytes(),
    );
    for (name, value) in headers {
        out.extend_from_slice(format!("{}: {}\r\n", name, value).as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// MIME by file extension. Unknown extensions fall back to
/// `application/octet-stream` so an unfamiliar file is never interpreted as a
/// document (the UI also sends `X-Content-Type-Options: nosniff`).
pub fn content_type(path: &str) -> &'static str {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_is_chosen_by_extension() {
        for (path, expected) in [
            ("index.html", "text/html; charset=utf-8"),
            ("INDEX.HTM", "text/html; charset=utf-8"),
            ("app.js", "text/javascript"),
            ("app.mjs", "text/javascript"),
            ("style.css", "text/css"),
            ("data.json", "application/json"),
            ("app.js.map", "application/json"),
            ("logo.svg", "image/svg+xml"),
            ("icon.png", "image/png"),
            ("photo.jpeg", "image/jpeg"),
            ("anim.gif", "image/gif"),
            ("image.webp", "image/webp"),
            ("favicon.ico", "image/x-icon"),
            ("app.wasm", "application/wasm"),
            ("font.woff2", "font/woff2"),
            ("font.ttf", "font/ttf"),
            ("readme.txt", "text/plain; charset=utf-8"),
            ("feed.xml", "application/xml"),
            ("doc.pdf", "application/pdf"),
            ("payload.bin", "application/octet-stream"),
            ("no_extension", "application/octet-stream"),
        ] {
            assert_eq!(content_type(path), expected, "{path}");
        }
    }
}
