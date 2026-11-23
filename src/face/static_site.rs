//! `/static` face: one configured directory as a read-only static site.
//!
//! `svcd.conf` names the root (`static_root`); this module owns the URL
//! projection. `/static/` and `/static/<dir>/` answer that directory's
//! `index.html`, a directory without the trailing slash redirects to the
//! canonical form so relative links resolve, and everything else is a strict
//! 404 (no SPA fallback, no directory listing).
//!
//! The root may contain a whole web build, so the rules are deliberately
//! conservative: every segment is percent-decoded before validation,
//! `.`/`..`/empty segments/backslashes are rejected, the resolved path must
//! canonicalize inside the root, and dotfiles/dot-directories are refused.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::face::asset::AssetStream;
use crate::face::plugin_ui::unauthenticated;
use crate::face::webui;
use crate::platform::config::Config;
use crate::server::auth::admission::{Admission, Rejected};
use crate::server::auth::lan::Lan;
use crate::server::http::model as http;
use crate::server::http::Exchange;

/// The URL prefix the configured root is mounted at.
pub(crate) const PREFIX: &str = "/static";

/// A resolved request target. `Directory` still needs its slash added by the
/// caller, which has the raw path (including any query).
#[derive(Debug)]
pub(crate) enum Target {
    File(PathBuf, fs::Metadata),
    Directory,
    Missing,
}

/// The request target without its query string.
pub(crate) fn path_only(path: &str) -> &str {
    path.split_once('?').map(|(path, _)| path).unwrap_or(path)
}

/// Whether this URL belongs to the static site prefix. The bare `/static` is
/// included so it can redirect to `/static/`.
pub(crate) fn is_path(path: &str) -> bool {
    let path = path_only(path);
    path == PREFIX || path.starts_with("/static/")
}

/// Resolve and authenticate one static-site request. `Err` is a complete
/// response to send before the face runs (auth, redirect, 404, 405).
pub(crate) fn exchange(
    config: &Config,
    head: &http::Head,
    lan: &Arc<Mutex<Lan>>,
) -> Result<Exchange, Vec<u8>> {
    match Admission::new(lan).admit(&head.headers) {
        Ok(_) => {}
        Err(Rejected::Unavailable) => {
            return Err(http::json_error(
                500,
                "internal",
                "authentication state unavailable",
            ))
        }
        Err(Rejected::Unauthorized) => {
            return Err(unauthenticated(&head.headers, path_only(&head.path)))
        }
    }
    if head.method != "GET" && head.method != "HEAD" {
        return Err(http::method_not_allowed("GET, HEAD"));
    }
    let Some(root) = config.settings.static_root.as_deref() else {
        return Err(not_found());
    };
    match resolve(root, path_only(&head.path)) {
        Target::File(path, metadata) => Ok(Exchange::Stream(Box::new(AssetStream::static_file(
            &head.headers,
            &path,
            &metadata,
            head.method == "HEAD",
        )))),
        Target::Directory => Err(redirect(&slash_location(&head.path))),
        Target::Missing => Err(not_found()),
    }
}

/// Map one URL path to a file or a directory redirect. This is the decision
/// half; `exchange` owns authentication and the response.
pub(crate) fn resolve(root: &Path, path: &str) -> Target {
    let Some(rest) = path.strip_prefix(PREFIX) else {
        return Target::Missing;
    };
    if rest.is_empty() {
        // `/static` canonicalises to `/static/`.
        return Target::Directory;
    }
    if !rest.starts_with('/') {
        return Target::Missing;
    }
    let Some(relative) = webui::relative_path(rest, "index.html") else {
        return Target::Missing;
    };
    if has_dotfile(&relative) {
        return Target::Missing;
    }
    if let Some(found) = webui::web_file(root, &relative) {
        return Target::File(found.0, found.1);
    }
    // A trailing-slash path already asked for `dir/index.html`; only the bare
    // directory form gets a redirect, so a missing index is a real 404.
    if !rest.ends_with('/') && webui::web_dir(root, &relative) {
        return Target::Directory;
    }
    Target::Missing
}

/// No component may start with a dot: `.git`, `.env` and an operator's editor
/// backups stay outside the served surface. `.` and `..` never reach here
/// because `webui::relative_path` rejects them first.
fn has_dotfile(relative: &str) -> bool {
    relative.split('/').any(|segment| segment.starts_with('.'))
}

fn slash_location(raw_path: &str) -> String {
    let (path, query) = match raw_path.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (raw_path, None),
    };
    match query {
        Some(query) => format!("{}/?{}", path, query),
        None => format!("{}/", path),
    }
}

fn redirect(location: &str) -> Vec<u8> {
    http::response_with_headers(
        308,
        "text/plain; charset=utf-8",
        &[
            ("Location".to_string(), location.to_string()),
            ("Cache-Control".to_string(), "no-store".to_string()),
        ],
        b"",
    )
}

fn not_found() -> Vec<u8> {
    http::json_error(404, "not_found", "not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> TempDir {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let name = format!(
                "svcd-static-site-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                nanos
            );
            let path = crate::test_support::temp_dir().join(name);
            fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, bytes).expect("write fixture");
    }

    #[test]
    fn the_prefix_is_claimed_exactly() {
        assert!(is_path("/static"));
        assert!(is_path("/static/"));
        assert!(is_path("/static/app.js?x=1"));
        assert!(!is_path("/staticish"));
        assert!(!is_path("/plugin-ui/static"));
    }

    #[test]
    fn index_is_served_for_the_root_and_for_directories() {
        let dir = TempDir::new();
        write(&dir.0, "index.html", b"root");
        write(&dir.0, "docs/index.html", b"docs");

        assert!(
            matches!(resolve(&dir.0, "/static/"), Target::File(path, _) if path.ends_with("index.html"))
        );
        assert!(
            matches!(resolve(&dir.0, "/static/docs/"), Target::File(path, _) if path.ends_with("docs/index.html"))
        );
    }

    #[test]
    fn a_directory_without_a_slash_is_a_redirect() {
        let dir = TempDir::new();
        write(&dir.0, "docs/index.html", b"docs");

        assert!(matches!(resolve(&dir.0, "/static/docs"), Target::Directory));
        assert!(matches!(resolve(&dir.0, "/static"), Target::Directory));
        assert_eq!(slash_location("/static/docs"), "/static/docs/");
        assert_eq!(slash_location("/static/docs?a=1"), "/static/docs/?a=1");
        // A trailing slash asks for the index; a missing index is not a
        // redirect back to the same URL.
        assert!(matches!(
            resolve(&dir.0, "/static/missing/"),
            Target::Missing
        ));
    }

    #[test]
    fn missing_files_do_not_fall_back_to_the_index() {
        let dir = TempDir::new();
        write(&dir.0, "index.html", b"root");

        assert!(matches!(resolve(&dir.0, "/static/app.js"), Target::Missing));
        assert!(matches!(
            resolve(&dir.0, "/static/deep/missing.js"),
            Target::Missing
        ));
    }

    #[test]
    fn traversal_and_dotfiles_are_refused() {
        let dir = TempDir::new();
        write(&dir.0, "index.html", b"root");
        write(&dir.0, "secret.txt", b"secret");
        write(&dir.0, ".env", b"secret");
        write(&dir.0, ".git/config", b"secret");

        for path in [
            "/static/../secret.txt",
            "/static/%2e%2e/secret.txt",
            "/static/%2E%2E/secret.txt",
            "/static/..%5Csecret.txt",
            "/static//secret.txt",
            "/static/.env",
            "/static/.git/config",
            "/static/%2e%2e%2fsecret.txt",
        ] {
            assert!(
                matches!(resolve(&dir.0, path), Target::Missing),
                "{path} must not resolve"
            );
        }
    }

    #[test]
    fn a_symlink_out_of_the_root_is_not_served() {
        #[cfg(unix)]
        {
            let dir = TempDir::new();
            let outside = TempDir::new();
            write(&outside.0, "outside.txt", b"outside");
            std::os::unix::fs::symlink(outside.0.join("outside.txt"), dir.0.join("link.txt"))
                .expect("symlink");
            assert!(matches!(
                resolve(&dir.0, "/static/link.txt"),
                Target::Missing
            ));
        }
    }
}
