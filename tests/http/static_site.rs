//! /static face: a configured directory served as a read-only static site.

use super::*;

fn write(root: &std::path::Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, bytes).expect("write fixture");
}

fn configure(sandbox: &Sandbox, root: &std::path::Path) {
    fs::write(
        sandbox.config.join("svcd.conf"),
        format!("static_root = {}\n", root.display()),
    )
    .expect("write svcd.conf");
}

const PAGE: &[u8] = b"<!doctype html>\n<html><body><a href=\"assets/app.js\">app</a></body></html>";

#[test]
fn serves_html_js_css_and_images_from_the_configured_root() {
    let mut sandbox = Sandbox::new();
    let web = sandbox.root.join("web");
    write(&web, "index.html", PAGE);
    write(&web, "assets/app.js", b"console.log(\"ok\");\n");
    write(&web, "assets/style.css", b"body{color:red}\n");
    write(
        &web,
        "icon.png",
        &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
    );
    configure(&sandbox, &web);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let page = raw_request(&address, "GET", "/static/", "", b"");
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));
    assert_eq!(
        raw_header_values(&page, "content-type"),
        vec!["text/html; charset=utf-8"]
    );
    // The static site does not inject a plugin-UI <base> tag.
    assert_eq!(page.body, PAGE);
    let etag = raw_header_values(&page, "etag")
        .first()
        .expect("static page etag")
        .to_string();

    let explicit = raw_request(&address, "GET", "/static/index.html", "", b"");
    assert_eq!(explicit.status, 200);
    assert_eq!(explicit.body, PAGE);

    let script = raw_request(&address, "GET", "/static/assets/app.js", "", b"");
    assert_eq!(script.status, 200);
    assert_eq!(
        raw_header_values(&script, "content-type"),
        vec!["text/javascript"]
    );
    assert_eq!(script.body, b"console.log(\"ok\");\n");

    let style = raw_request(&address, "GET", "/static/assets/style.css", "", b"");
    assert_eq!(style.status, 200);
    assert_eq!(raw_header_values(&style, "content-type"), vec!["text/css"]);

    let icon = raw_request(&address, "GET", "/static/icon.png", "", b"");
    assert_eq!(icon.status, 200);
    assert_eq!(raw_header_values(&icon, "content-type"), vec!["image/png"]);

    let revalidated = raw_request_with(
        &address,
        "GET",
        "/static/index.html",
        "",
        b"",
        &[("If-None-Match", etag.as_str())],
    );
    assert_eq!(revalidated.status, 304);
    assert!(revalidated.body.is_empty());
    assert_eq!(raw_header_values(&revalidated, "etag"), vec![etag.as_str()]);
}

#[test]
fn redirects_directories_and_refuses_unsafe_or_missing_paths() {
    let mut sandbox = Sandbox::new();
    let web = sandbox.root.join("web");
    write(&web, "index.html", PAGE);
    write(&web, "docs/index.html", b"docs");
    write(&web, ".env", b"secret");
    configure(&sandbox, &web);
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    // A real directory without the trailing slash gets the canonical URL so
    // relative links inside its index resolve under /static/docs/.
    let redirect = raw_request(&address, "GET", "/static/docs", "", b"");
    assert_eq!(redirect.status, 308);
    assert_eq!(
        raw_header_values(&redirect, "location"),
        vec!["/static/docs/"]
    );

    let index = raw_request(&address, "GET", "/static/docs/", "", b"");
    assert_eq!(index.status, 200);
    assert_eq!(index.body, b"docs");

    // A missing script is a 404 even when the browser would accept HTML:
    // there is no SPA fallback on the static site.
    for path in ["/static/missing.js", "/static/nested/missing.css"] {
        let missing = raw_request_with(&address, "GET", path, "", b"", &[("Accept", "text/html")]);
        assert_eq!(missing.status, 404, "{path}");
    }

    for path in [
        "/static/../Cargo.toml",
        "/static/%2e%2e/Cargo.toml",
        "/static/.env",
        "/static//Cargo.toml",
    ] {
        let refused = raw_request(&address, "GET", path, "", b"");
        assert_eq!(refused.status, 404, "{path}");
    }

    let post = raw_request(&address, "POST", "/static/index.html", "", b"");
    assert_eq!(post.status, 405);
    assert_eq!(raw_header_values(&post, "allow"), vec!["GET, HEAD"]);

    let head = raw_request(&address, "HEAD", "/static/index.html", "", b"");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    let length = PAGE.len().to_string();
    assert_eq!(
        raw_header_values(&head, "content-length"),
        vec![length.as_str()]
    );
}

#[test]
fn static_root_is_off_until_configured() {
    let mut sandbox = Sandbox::new();
    sandbox.start_daemon();
    let address = sandbox.wait_for_http();

    let missing = raw_request(&address, "GET", "/static/index.html", "", b"");
    assert_eq!(missing.status, 404);
}

#[test]
fn static_files_follow_the_daemons_authentication() {
    let mut sandbox = Sandbox::new();
    let web = sandbox.root.join("web");
    write(&web, "index.html", PAGE);
    configure(&sandbox, &web);
    sandbox.start_daemon();
    sandbox.wait_for_http();
    let (address, password) = enable_auth(&mut sandbox);

    let denied = raw_request_with(
        &address,
        "GET",
        "/static/index.html",
        "",
        b"",
        &[("Accept", "text/html")],
    );
    assert_eq!(denied.status, 303);
    assert_eq!(
        raw_header_values(&denied, "location"),
        vec!["/?next=/static/index.html"]
    );

    let bearer = format!("Bearer {}", password);
    let allowed = raw_request_with(
        &address,
        "GET",
        "/static/index.html",
        "",
        b"",
        &[("Authorization", bearer.as_str())],
    );
    assert_eq!(allowed.status, 200);
    assert_eq!(allowed.body, PAGE);
}
