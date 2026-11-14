//! Content-addressed blob store for payloads that do not belong in a frame.
//!
//! A tool argument may carry `{"$blob":"sha256:<hex>"}`. The daemon resolves
//! that to a path inside this store before forwarding it to a plugin, so a
//! multi-megabyte file never has to fit in a JSON-RPC frame. Blobs can also be
//! uploaded and downloaded through the management API.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::settings;

pub const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    pub size: u64,
}

pub fn dir() -> PathBuf {
    paths::temp_dir()
}

pub fn incoming_dir() -> PathBuf {
    dir().join("incoming")
}

pub fn put(bytes: &[u8]) -> Result<Meta, String> {
    if bytes.len() as u64 > MAX_BLOB_BYTES {
        return Err("blob is too large".into());
    }
    let digest = Sha256::digest(bytes);
    let id = format!("sha256:{}", hex(&digest));
    let directory = dir();
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    let path = blob_path(&id)?;
    if !path.is_file() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = directory.join(format!(".tmp-{}-{}", std::process::id(), nanos));
        let mut file = fs::File::create(&temp)
            .map_err(|error| format!("create {}: {}", temp.display(), error))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {}: {}", temp.display(), error))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {}", temp.display(), error))?;
        fs::rename(&temp, &path)
            .map_err(|error| format!("rename {}: {}", path.display(), error))?;
    }
    Ok(Meta {
        id,
        size: bytes.len() as u64,
    })
}

pub fn read(id: &str) -> Result<Vec<u8>, String> {
    let path = blob_path(id)?;
    touch(&path);
    fs::read(&path).map_err(|error| format!("read {}: {}", path.display(), error))
}

pub fn path(id: &str) -> Result<PathBuf, String> {
    let path = blob_path(id)?;
    touch(&path);
    Ok(path)
}

/// Remove temp files older than `retention`. This includes content-addressed
/// blobs and stale `incoming/` files. Active reads/argument resolution touch
/// the mtime first, so a file that is being used is not immediately eligible.
pub fn cleanup(retention: Duration) -> std::io::Result<usize> {
    let root = dir();
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

fn cleanup_dir(
    root: &Path,
    directory: &Path,
    cutoff: SystemTime,
    removed: &mut usize,
) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            cleanup_dir(root, &path, cutoff, removed)?;
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
        if modified <= cutoff {
            if fs::remove_file(&path).is_ok() {
                *removed += 1;
            }
        }
    }
    Ok(())
}

/// Start one background cleanup thread. `temp_cleanup_interval_secs = 0`
/// disables it.
pub fn spawn_cleanup() {
    let Ok(settings) = settings::current() else {
        return;
    };
    let retention = settings.temp_retention();
    let interval = settings.temp_cleanup_interval();
    if interval.is_zero() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("svcd-temp-cleanup".into())
        .stack_size(128 * 1024)
        .spawn(move || loop {
            match cleanup(retention) {
                Ok(0) => {}
                Ok(removed) => {
                    crate::log::svcd(&format!("svcd: temp cleanup removed {} file(s)", removed))
                }
                Err(error) => crate::log::svcd(&format!("svcd: temp cleanup failed: {}", error)),
            }
            std::thread::sleep(interval);
        });
}

fn touch(path: &Path) {
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
    }
}

/// Import a file a plugin wrote into `SVCD_TEMP_DIR` and replace a
/// `$blobPath` result with an MCP resource link plus structured blob metadata.
pub fn materialize_result(result: &mut Value) -> Result<(), String> {
    let Some(path) = result.get("$blobPath").and_then(Value::as_str) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let meta = import_file(&path)?;
    let name = result.get("name").and_then(Value::as_str).unwrap_or("blob");
    let mime = result
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    *result = json!({
        "content": [
            {"type": "text", "text": format!("blob {}", meta.id)},
            {
                "type": "resource_link",
                "uri": format!("svcd://blob/{}", meta.id),
                "name": name,
                "mimeType": mime
            }
        ],
        "structuredContent": {
            "$blob": meta.id,
            "size": meta.size
        },
        "isError": false
    });
    Ok(())
}

fn import_file(source: &Path) -> Result<Meta, String> {
    let directory = dir();
    let incoming = incoming_dir();
    fs::create_dir_all(&incoming)
        .map_err(|error| format!("create {}: {}", incoming.display(), error))?;
    let source = source
        .canonicalize()
        .map_err(|error| format!("open {}: {}", source.display(), error))?;
    let incoming = incoming
        .canonicalize()
        .map_err(|error| format!("open {}: {}", incoming.display(), error))?;
    if !source.starts_with(&incoming) {
        return Err("blob path is outside the incoming directory".into());
    }
    let metadata =
        fs::metadata(&source).map_err(|error| format!("stat {}: {}", source.display(), error))?;
    if metadata.len() > MAX_BLOB_BYTES {
        return Err("blob is too large".into());
    }
    let mut file =
        fs::File::open(&source).map_err(|error| format!("open {}: {}", source.display(), error))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = directory.join(format!(".incoming-{}-{}", std::process::id(), nanos));
    let mut output =
        fs::File::create(&temp).map_err(|error| format!("create {}: {}", temp.display(), error))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("read {}: {}", source.display(), error))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > MAX_BLOB_BYTES {
            let _ = fs::remove_file(&temp);
            return Err("blob is too large".into());
        }
        hasher.update(&chunk[..read]);
        output
            .write_all(&chunk[..read])
            .map_err(|error| format!("write {}: {}", temp.display(), error))?;
    }
    output
        .sync_all()
        .map_err(|error| format!("sync {}: {}", temp.display(), error))?;
    let id = format!("sha256:{}", hex(&hasher.finalize()));
    let destination = blob_path(&id)?;
    if destination.is_file() {
        let _ = fs::remove_file(&temp);
    } else {
        fs::rename(&temp, &destination)
            .map_err(|error| format!("rename {}: {}", destination.display(), error))?;
    }
    let _ = fs::remove_file(&source);
    Ok(Meta { id, size })
}

pub fn meta(id: &str) -> Result<Meta, String> {
    let path = blob_path(id)?;
    let size = fs::metadata(&path)
        .map_err(|error| format!("stat {}: {}", path.display(), error))?
        .len();
    Ok(Meta {
        id: id.to_string(),
        size,
    })
}

/// Resolve `$blob` references anywhere in a tool-call argument tree. The
/// plugin receives `{ "$blob": id, "path": absolute_path, "size": n }`, which
/// is explicit about what happened and keeps the path out of the client's JSON.
pub fn resolve_args(value: &mut Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("$blob") {
                let meta = meta(id)?;
                let path = path(id)?;
                *value = json!({
                    "$blob": id,
                    "path": path.to_string_lossy(),
                    "size": meta.size,
                });
                return Ok(());
            }
            for child in map.values_mut() {
                resolve_args(child)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                resolve_args(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn blob_path(id: &str) -> Result<PathBuf, String> {
    let Some(hex) = id.strip_prefix("sha256:") else {
        return Err("invalid blob id".into());
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid blob id".into());
    }
    Ok(dir().join(hex))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{:02x}", byte);
    }
    text
}

pub fn content_type() -> &'static str {
    "application/octet-stream"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    fn root() -> &'static Path {
        ROOT.get_or_init(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("svcd-blob-{}-{}", std::process::id(), nanos));
            std::env::set_var("SVCD_DATA_DIR", &root);
            std::fs::create_dir_all(&root).expect("blob root");
            root
        })
    }

    #[test]
    fn blobs_are_content_addressed_and_readable() {
        let _ = root();
        let first = put(b"hello").expect("put");
        let second = put(b"hello").expect("put");
        assert_eq!(first, second);
        assert_eq!(read(&first.id).expect("read"), b"hello");
        assert!(path(&first.id).expect("path").is_file());
    }

    #[test]
    fn invalid_ids_are_rejected() {
        for id in ["", "sha256:", "sha256:xyz", "../x", "sha256:../x"] {
            assert!(read(id).is_err(), "{id}");
        }
    }

    #[test]
    fn a_plugin_blob_result_becomes_a_resource_link() {
        let _ = root();
        let incoming = incoming_dir();
        std::fs::create_dir_all(&incoming).expect("incoming");
        let source = incoming.join("report.txt");
        std::fs::write(&source, b"result body").expect("result");
        let mut result = json!({
            "$blobPath": source.to_string_lossy(),
            "name": "report.txt",
            "mimeType": "text/plain"
        });
        materialize_result(&mut result).expect("materialize");
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][1]["type"], "resource_link");
        assert!(result["structuredContent"]["$blob"]
            .as_str()
            .expect("blob id")
            .starts_with("sha256:"));
        assert!(!source.exists());
        let id = result["structuredContent"]["$blob"]
            .as_str()
            .expect("blob id");
        assert_eq!(read(id).expect("read"), b"result body");
    }

    #[test]
    fn cleanup_removes_old_files_and_keeps_fresh_ones() {
        let _ = root();
        let incoming = incoming_dir();
        std::fs::create_dir_all(&incoming).expect("incoming");
        let old = incoming.join("old.bin");
        let fresh = incoming.join("fresh.bin");
        std::fs::write(&old, b"old").expect("old");
        std::fs::write(&fresh, b"fresh").expect("fresh");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&old)
            .expect("open old");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(3 * 60 * 60)),
        )
        .expect("old mtime");
        let removed = cleanup(Duration::from_secs(2 * 60 * 60)).expect("cleanup");
        assert!(removed >= 1, "removed {removed}");
        assert!(!old.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn blob_refs_are_resolved_to_paths_in_arguments() {
        let _ = root();
        let meta = put(b"file body").expect("put");
        let mut args = json!({"input": {"$blob": meta.id}, "other": 1});
        resolve_args(&mut args).expect("resolve");
        let input = &args["input"];
        assert_eq!(input["$blob"], meta.id);
        assert_eq!(input["size"], 9);
        assert!(input["path"].as_str().expect("path").contains("temp"));
    }
}
