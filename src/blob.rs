//! Content-addressed blob store for payloads that do not belong in a frame.
//!
//! A tool argument may carry `{"$blob":"sha256:<hex>"}`. The daemon resolves
//! that to a path inside this store before forwarding it to a plugin, so a
//! multi-megabyte file never has to fit in a JSON-RPC frame. Blobs can also be
//! uploaded and downloaded through the management API.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::paths;

pub const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    pub size: u64,
}

pub fn dir() -> PathBuf {
    paths::data_dir().join("blobs")
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
    fs::read(&path).map_err(|error| format!("read {}: {}", path.display(), error))
}

pub fn path(id: &str) -> Result<PathBuf, String> {
    blob_path(id)
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
    fn blob_refs_are_resolved_to_paths_in_arguments() {
        let _ = root();
        let meta = put(b"file body").expect("put");
        let mut args = json!({"input": {"$blob": meta.id}, "other": 1});
        resolve_args(&mut args).expect("resolve");
        let input = &args["input"];
        assert_eq!(input["$blob"], meta.id);
        assert_eq!(input["size"], 9);
        assert!(input["path"].as_str().expect("path").contains("blobs"));
    }
}
