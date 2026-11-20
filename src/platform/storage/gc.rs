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
