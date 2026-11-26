//! 过期回收：按 mtime 清理临时区、上传会话与不再引用的内容。

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::platform::paths::Paths;

/// Remove temp files older than `retention`. This includes content-addressed
/// blobs and stale `incoming/` files. Active reads/argument resolution touch
/// the mtime first, so a file that is being used is not immediately eligible.
pub(crate) fn cleanup(paths: &Paths, retention: Duration) -> std::io::Result<usize> {
    let root = paths.temp_dir.clone();
    if !root.is_dir() {
        return Ok(0);
    }
    let cutoff = SystemTime::now()
        .checked_sub(retention)
        .unwrap_or(UNIX_EPOCH);
    let mut removed = 0;
    cleanup_dir(&root, &root, cutoff, &mut removed)?;
    Ok(removed)
}

/// Release the content-addressed entries a consumed transfer created.
///
/// A package install is a one-shot input: the plugin store copies the package
/// into `<temp>/incoming`, the host imports it as the `sha256:<hex>` blob the
/// browser downloads, and then the install POST carries the same bytes back.
/// Once the attempt ends nothing references that entry, so waiting for
/// `temp_retention_secs` (two hours) pins the package on a small volume
/// (#288). A missing entry is already clean, not an error.
#[cfg(feature = "http")]
pub(crate) fn release(paths: &Paths, digest: &[u8]) -> usize {
    release_id(paths, &format!("sha256:{}", text_hex(digest)))
}

/// Remove one content-addressed entry by id: the single blob and the file
/// bundle that shares its digest (a bundle is the directory, a blob the file).
///
/// The id is the same handle the caller was given, so a handover that never
/// reached a consumer can be released without re-hashing the bytes (#302).
#[cfg(feature = "http")]
pub(crate) fn release_id(paths: &Paths, id: &str) -> usize {
    let mut removed = 0;
    if let Ok(blob) = super::path::blob_path(paths, id) {
        if fs::remove_file(&blob).is_ok() {
            removed += 1;
        }
    }
    if let Ok(directory) = super::path::file_dir(paths, id) {
        if directory.is_dir() && fs::remove_dir_all(&directory).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Remove one blob entry by id. `false` when nothing was there, so the route
/// can answer 404 without inventing an error (#291).
#[cfg(feature = "http")]
pub(crate) fn forget_blob(paths: &Paths, id: &str) -> bool {
    match super::path::blob_path(paths, id) {
        Ok(path) => fs::remove_file(path).is_ok(),
        // A malformed id names nothing, which is the same answer as absent.
        Err(_) => false,
    }
}

/// Remove one `files/<id>` bundle. The manifest and its parts are one unit.
#[cfg(feature = "http")]
pub(crate) fn forget_file(paths: &Paths, id: &str) -> bool {
    match super::path::file_dir(paths, id) {
        Ok(directory) if directory.is_dir() => fs::remove_dir_all(directory).is_ok(),
        _ => false,
    }
}

#[cfg(feature = "http")]
fn text_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

pub(crate) fn cleanup_dir(
    root: &Path,
    directory: &Path,
    cutoff: SystemTime,
    removed: &mut usize,
) -> std::io::Result<()> {
    // A file bundle or an in-progress part session is one unit: its marker is
    // the TTL clock, not each individual part. This keeps a long upload or a
    // file that is still referenced from losing some of its parts.
    let managed = directory
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .map(|name| name == "uploads" || name == "files")
        .unwrap_or(false);
    if managed {
        let marker = ["manifest.json", "session"]
            .into_iter()
            .map(|name| directory.join(name))
            .find(|path| path.is_file());
        if let Some(marker) = marker {
            let modified = fs::metadata(&marker)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            if modified <= cutoff && fs::remove_dir_all(directory).is_ok() {
                *removed += 1;
            }
            return Ok(());
        }
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        // Cleanup runs concurrently with uploads and other cleanup passes; a
        // file that disappears between read_dir and stat is not an error.
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            match cleanup_dir(root, &path, cutoff, removed) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            if path != root
                && path
                    .file_name()
                    .map(|name| name != "incoming")
                    .unwrap_or(false)
            {
                let _ = fs::remove_dir(&path);
            }
            continue;
        }
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        if modified <= cutoff && fs::remove_file(&path).is_ok() {
            *removed += 1;
        }
    }
    Ok(())
}
