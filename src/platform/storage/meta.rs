//! 存储值类型与 `$blob` 引用：内容元数据、文件清单，以及调用参数/结果的物化。

use std::fs;
use std::path::PathBuf;

#[cfg(feature = "http")]
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::platform::paths::Paths;

use super::path::{blob_path, import_file, path};
#[cfg(feature = "http")]
use super::path::file_dir;
#[cfg(feature = "http")]
use super::upload::file_meta;
use super::{Error, Result};

#[derive(Debug, PartialEq, Eq)]
pub struct Meta {
    pub id: String,
    pub size: u64,
}

/// One part entry in a file manifest.
#[cfg(feature = "http")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilePart {
    pub number: u32,
    pub size: u64,
    pub sha256: String,
}

/// On-disk manifest for a logical file. The file itself is the ordered list of
/// part files; it is never concatenated into a second copy.
#[cfg(feature = "http")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileManifest {
    pub schema: u32,
    pub id: String,
    pub size: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_mime")]
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub parts: Vec<FilePart>,
}

#[cfg(feature = "http")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub id: String,
    pub size: u64,
    pub name: String,
    pub mime_type: String,
}

#[cfg(feature = "http")]
pub(crate) fn default_mime() -> String {
    "application/octet-stream".to_string()
}

/// Import a file a plugin wrote into `SVCD_TEMP_DIR` and replace a
/// `$blobPath` result with an MCP resource link plus structured blob metadata.
pub(crate) fn materialize_result(paths: &Paths, result: &mut Value) -> Result<()> {
    let Some(path) = result.get("$blobPath").and_then(Value::as_str) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    let meta = import_file(paths, &path)?;
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

pub(crate) fn meta(paths: &Paths, id: &str) -> Result<Meta> {
    let path = blob_path(paths, id)?;
    let size = fs::metadata(&path)
        .map_err(|error| Error::internal(format!("stat {}: {}", path.display(), error)))?
        .len();
    Ok(Meta {
        id: id.to_string(),
        size,
    })
}

/// Resolve `$blob` references anywhere in a tool-call argument tree. The
/// plugin receives `{ "$blob": id, "path": absolute_path, "size": n }`, which
/// is explicit about what happened and keeps the path out of the client's JSON.
pub(crate) fn resolve_args(paths: &Paths, value: &mut Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(id)) = map.get("$blob") {
                let meta = meta(paths, id)?;
                let path = path(paths, id)?;
                *value = json!({
                    "$blob": id,
                    "path": path.to_string_lossy(),
                    "size": meta.size,
                });
                return Ok(());
            }
            #[cfg(feature = "http")]
            if let Some(Value::String(id)) = map.get("$file") {
                let meta = file_meta(paths, id)?;
                let directory = file_dir(paths, id)?;
                let manifest = directory.join("manifest.json");
                *value = json!({
                    "$file": id,
                    "dir": directory.to_string_lossy(),
                    "manifest": manifest.to_string_lossy(),
                    "size": meta.size,
                    "name": meta.name,
                    "mimeType": meta.mime_type,
                });
                return Ok(());
            }
            for child in map.values_mut() {
                resolve_args(paths, child)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                resolve_args(paths, child)?;
            }
        }
        _ => {}
    }
    Ok(())
}
