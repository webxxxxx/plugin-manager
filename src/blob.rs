//! Content-addressed blob store for payloads that do not belong in a frame.
//!
//! A tool argument may carry `{"$blob":"sha256:<hex>"}`. The daemon resolves
//! that to a path inside this store before forwarding it to a plugin, so a
//! multi-megabyte file never has to fit in a JSON-RPC frame. Blobs can also be
//! uploaded and downloaded through the management API.

use std::fs;
use std::io::Read;
#[cfg(any(feature = "http", test))]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "http", test))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "http")]
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(feature = "http")]
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::paths;
use crate::settings;

pub const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;
/// A file bundle is made of parts; the existing blob cap is the per-part cap.
#[cfg(any(feature = "http", test))]
pub const MAX_FILE_PART_BYTES: u64 = MAX_BLOB_BYTES;
/// Safety net for one logical file; the benchmark ISO is 1.59 GiB.
#[cfg(feature = "http")]
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024 * 1024;
#[cfg(feature = "http")]
pub const MAX_PART_NUMBER: u32 = 10_000;

#[cfg(any(feature = "http", test))]
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "http")]
static SESSION_LOCK: Mutex<()> = Mutex::new(());

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

/// A streaming write into the content-addressed store. Bytes are hashed while
/// they land in a temp file, so a large upload never has to fit in memory.
#[cfg(any(feature = "http", test))]
pub struct Upload {
    file: fs::File,
    hasher: Sha256,
    temp: PathBuf,
    size: u64,
    committed: bool,
}

#[cfg(any(feature = "http", test))]
impl Upload {
    pub fn begin() -> Result<Upload, String> {
        let directory = dir();
        fs::create_dir_all(&directory)
            .map_err(|error| format!("create {}: {}", directory.display(), error))?;
        let temp = directory.join(format!(".upload-{}-{}", std::process::id(), next_temp()));
        let file = fs::File::create(&temp)
            .map_err(|error| format!("create {}: {}", temp.display(), error))?;
        Ok(Upload {
            file,
            hasher: Sha256::new(),
            temp,
            size: 0,
            committed: false,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let size = self.size.saturating_add(bytes.len() as u64);
        if size > MAX_FILE_PART_BYTES {
            return Err("blob is too large".into());
        }
        self.file
            .write_all(bytes)
            .map_err(|error| format!("write {}: {}", self.temp.display(), error))?;
        self.hasher.update(bytes);
        self.size = size;
        Ok(())
    }

    pub fn finish(mut self) -> Result<Meta, String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync {}: {}", self.temp.display(), error))?;
        let digest = std::mem::replace(&mut self.hasher, Sha256::new()).finalize();
        let id = format!("sha256:{}", hex(&digest));
        let destination = blob_path(&id)?;
        if destination.is_file() {
            let _ = fs::remove_file(&self.temp);
        } else if let Err(error) = fs::rename(&self.temp, &destination) {
            // A concurrent upload may have won the race; both writers produced
            // the same content, so only the loser's temp file remains.
            if destination.is_file() {
                let _ = fs::remove_file(&self.temp);
            } else {
                return Err(format!("rename {}: {}", destination.display(), error));
            }
        }
        self.committed = true;
        Ok(Meta {
            id,
            size: self.size,
        })
    }
}

#[cfg(any(feature = "http", test))]
impl Drop for Upload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

#[cfg(test)]
pub fn put(bytes: &[u8]) -> Result<Meta, String> {
    let mut upload = Upload::begin()?;
    upload.write(bytes)?;
    upload.finish()
}

#[cfg(test)]
pub fn read(id: &str) -> Result<Vec<u8>, String> {
    let path = blob_path(id)?;
    touch(&path);
    fs::read(&path).map_err(|error| format!("read {}: {}", path.display(), error))
}

pub fn locate(id: &str) -> Result<PathBuf, String> {
    blob_path(id)
}

pub fn path(id: &str) -> Result<PathBuf, String> {
    let path = locate(id)?;
    touch(&path);
    Ok(path)
}

/// One committed part of a resumable upload session.
#[cfg(feature = "http")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInfo {
    pub number: u32,
    pub size: u64,
    pub etag: String,
}

/// A part session is created for exactly one commit mode. Blob sessions are
/// concatenated into one content-addressed blob; file sessions are published as
/// a manifest-backed file bundle.
#[cfg(feature = "http")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadKind {
    Blob,
    File,
}

#[cfg(feature = "http")]
impl UploadKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadKind::Blob => "blob",
            UploadKind::File => "file",
        }
    }
}

#[cfg(feature = "http")]
pub fn uploads_dir() -> PathBuf {
    dir().join("uploads")
}

#[cfg(feature = "http")]
pub fn valid_upload_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(feature = "http")]
pub fn upload_dir(id: &str) -> Result<PathBuf, String> {
    if !valid_upload_id(id) {
        return Err("invalid upload id".into());
    }
    Ok(uploads_dir().join(id))
}

/// Start a resumable upload session. The marker file keeps the directory from
/// being pruned as "empty" until the normal temp retention expires, and records
/// the commit mode so the other API cannot consume it.
#[cfg(feature = "http")]
pub fn create_upload(kind: UploadKind) -> Result<String, String> {
    let id = random_id()?;
    let directory = upload_dir(&id)?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    fs::write(directory.join("session"), kind.as_str().as_bytes())
        .map_err(|error| format!("write session marker: {}", error))?;
    Ok(id)
}

#[cfg(feature = "http")]
fn read_upload_kind(id: &str) -> Result<UploadKind, String> {
    let directory = upload_dir(id)?;
    if !directory.is_dir() {
        return Err("unknown upload".into());
    }
    let marker = fs::read_to_string(directory.join("session")).unwrap_or_default();
    match marker.trim() {
        "blob" => Ok(UploadKind::Blob),
        "file" => Ok(UploadKind::File),
        // Sessions created before kind markers existed were blob sessions.
        "" => Ok(UploadKind::Blob),
        _ => Err("unknown upload kind".into()),
    }
}

#[cfg(feature = "http")]
pub fn require_upload_kind(id: &str, expected: UploadKind) -> Result<(), String> {
    let actual = read_upload_kind(id)?;
    if actual != expected {
        return Err("upload kind mismatch".into());
    }
    Ok(())
}

#[cfg(feature = "http")]
pub fn upload_parts(id: &str, kind: UploadKind) -> Result<Vec<PartInfo>, String> {
    require_upload_kind(id, kind)?;
    let directory = upload_dir(id)?;
    if !directory.is_dir() {
        return Err("unknown upload".into());
    }
    let mut parts = Vec::new();
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("read {}: {}", directory.display(), error))?
    {
        let entry = entry.map_err(|error| format!("read {}: {}", directory.display(), error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(digits) = name.strip_prefix("part-") else {
            continue;
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(number) = digits.parse::<u32>() else {
            continue;
        };
        if number == 0 || number > MAX_PART_NUMBER {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("stat {}: {}", name, error))?;
        if !metadata.is_file() {
            continue;
        }
        let etag = fs::read_to_string(directory.join(format!("part-{}.etag", number)))
            .unwrap_or_default()
            .trim()
            .to_string();
        parts.push(PartInfo {
            number,
            size: metadata.len(),
            etag,
        });
    }
    parts.sort_by_key(|part| part.number);
    Ok(parts)
}

/// Streaming writer for one part of a resumable upload. Uploading the same
/// part number again replaces it, which makes retries idempotent.
#[cfg(feature = "http")]
pub struct PartWriter {
    file: fs::File,
    hasher: Sha256,
    temp: PathBuf,
    part: PathBuf,
    etag_path: PathBuf,
    number: u32,
    size: u64,
    committed: bool,
}

#[cfg(feature = "http")]
impl PartWriter {
    pub fn begin(upload_id: &str, number: u32, kind: UploadKind) -> Result<PartWriter, String> {
        if number == 0 || number > MAX_PART_NUMBER {
            return Err("part number must be between 1 and 10000".into());
        }
        require_upload_kind(upload_id, kind)?;
        let directory = upload_dir(upload_id)?;
        if !directory.is_dir() {
            return Err("unknown upload".into());
        }
        // Keep the whole session alive even when an early part is older than
        // the retention window by the time the upload finishes.
        touch(&directory.join("session"));
        let temp = directory.join(format!(".part-{}-{}.tmp", number, next_temp()));
        let file = fs::File::create(&temp)
            .map_err(|error| format!("create {}: {}", temp.display(), error))?;
        Ok(PartWriter {
            file,
            hasher: Sha256::new(),
            part: directory.join(format!("part-{}", number)),
            etag_path: directory.join(format!("part-{}.etag", number)),
            temp,
            number,
            size: 0,
            committed: false,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        let size = self.size.saturating_add(bytes.len() as u64);
        if size > MAX_FILE_PART_BYTES {
            return Err("blob is too large".into());
        }
        self.file
            .write_all(bytes)
            .map_err(|error| format!("write {}: {}", self.temp.display(), error))?;
        self.hasher.update(bytes);
        self.size = size;
        Ok(())
    }

    pub fn finish(mut self) -> Result<PartInfo, String> {
        self.file
            .sync_all()
            .map_err(|error| format!("sync {}: {}", self.temp.display(), error))?;
        let etag = format!(
            "sha256:{}",
            hex(&std::mem::replace(&mut self.hasher, Sha256::new()).finalize())
        );
        replace_file(&self.temp, &self.part)?;
        let temp_etag = self.etag_path.with_file_name(format!(
            ".part-{}-etag-{}.tmp",
            self.number,
            next_temp()
        ));
        fs::write(&temp_etag, etag.as_bytes())
            .map_err(|error| format!("write {}: {}", temp_etag.display(), error))?;
        replace_file(&temp_etag, &self.etag_path)?;
        self.committed = true;
        Ok(PartInfo {
            number: self.number,
            size: self.size,
            etag,
        })
    }
}

#[cfg(feature = "http")]
impl Drop for PartWriter {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

/// Concatenate the selected parts in ascending order into one content-addressed
/// blob. `None` selects every part that exists; an explicit list must be
/// strictly increasing and may omit parts. The session is removed on success.
#[cfg(feature = "http")]
pub fn complete_upload(
    id: &str,
    requested: Option<Vec<(u32, Option<String>)>>,
) -> Result<Meta, String> {
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = upload_dir(id)?;
    if !directory.is_dir() {
        return Err("unknown upload".into());
    }
    let available = upload_parts(id, UploadKind::Blob)?;
    let selected = select_parts(&available, requested)?;
    if selected.is_empty() {
        return Err("no parts uploaded".into());
    }
    let total: u64 = selected.iter().map(|part| part.size).sum();
    if total > MAX_BLOB_BYTES {
        return Err("blob is too large".into());
    }
    let mut upload = Upload::begin()?;
    let mut buffer = [0u8; 64 * 1024];
    for part in selected {
        let path = directory.join(format!("part-{}", part.number));
        let mut file =
            fs::File::open(&path).map_err(|error| format!("open {}: {}", path.display(), error))?;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| format!("read {}: {}", path.display(), error))?;
            if read == 0 {
                break;
            }
            upload.write(&buffer[..read])?;
        }
    }
    let meta = upload.finish()?;
    let _ = fs::remove_dir_all(&directory);
    Ok(meta)
}

#[cfg(feature = "http")]
fn select_parts(
    available: &[PartInfo],
    requested: Option<Vec<(u32, Option<String>)>>,
) -> Result<Vec<PartInfo>, String> {
    let Some(list) = requested else {
        return Ok(available.to_vec());
    };
    if list.is_empty() {
        return Err("parts is empty".into());
    }
    let mut selected = Vec::new();
    let mut previous = 0u32;
    for (number, etag) in list {
        if number == 0 || number > MAX_PART_NUMBER {
            return Err("part number must be between 1 and 10000".into());
        }
        if number <= previous {
            return Err("parts must be strictly increasing".into());
        }
        let Some(part) = available.iter().find(|part| part.number == number) else {
            return Err(format!("part {} is missing", number));
        };
        if let Some(etag) = etag {
            if normalize_etag(&etag) != normalize_etag(&part.etag) {
                return Err(format!("part {} etag does not match", number));
            }
        }
        selected.push(part.clone());
        previous = number;
    }
    Ok(selected)
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
fn default_mime() -> String {
    "application/octet-stream".to_string()
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
pub fn files_dir() -> PathBuf {
    dir().join("files")
}

#[cfg(feature = "http")]
pub fn file_dir(id: &str) -> Result<PathBuf, String> {
    Ok(files_dir().join(id_hex(id)?))
}

#[cfg(feature = "http")]
pub fn file_manifest(id: &str) -> Result<(PathBuf, FileManifest), String> {
    let directory = file_dir(id)?;
    let path = directory.join("manifest.json");
    let bytes = fs::read(&path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    let manifest: FileManifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {}", path.display(), error))?;
    if manifest.id != id {
        return Err(format!("manifest id mismatch for {}", id));
    }
    let size: u64 = manifest.parts.iter().map(|part| part.size).sum();
    if size != manifest.size {
        return Err(format!("manifest size mismatch for {}", id));
    }
    Ok((directory, manifest))
}

#[cfg(feature = "http")]
pub fn file_meta(id: &str) -> Result<FileMeta, String> {
    let (_, manifest) = file_manifest(id)?;
    Ok(FileMeta {
        id: manifest.id,
        size: manifest.size,
        name: manifest.name,
        mime_type: manifest.mime_type,
    })
}

/// Turn a completed part session into a logical file. Unlike `complete_upload`
/// this does not write a second copy: it hashes the parts in place and renames
/// the session directory into the file store.
#[cfg(feature = "http")]
pub fn complete_file(
    upload_id: &str,
    requested: Option<Vec<(u32, Option<String>)>>,
    name: Option<String>,
    mime_type: Option<String>,
) -> Result<FileMeta, String> {
    let directory = upload_dir(upload_id)?;
    if !directory.is_dir() {
        return Err("unknown upload".into());
    }
    let available = upload_parts(upload_id, UploadKind::File)?;
    let selected = select_parts(&available, requested)?;
    if selected.is_empty() {
        return Err("no parts uploaded".into());
    }
    let declared: u64 = selected.iter().map(|part| part.size).sum();
    if declared > MAX_FILE_BYTES {
        return Err("file is too large".into());
    }

    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    for part in &selected {
        let path = directory.join(format!("part-{}", part.number));
        let mut file =
            fs::File::open(&path).map_err(|error| format!("open {}: {}", path.display(), error))?;
        let actual = file
            .metadata()
            .map_err(|error| format!("stat {}: {}", path.display(), error))?
            .len();
        if actual != part.size {
            return Err("part sizes changed while completing the file".into());
        }
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| format!("read {}: {}", path.display(), error))?;
            if read == 0 {
                break;
            }
            size = size.saturating_add(read as u64);
            if size > MAX_FILE_BYTES {
                return Err("file is too large".into());
            }
            hasher.update(&buffer[..read]);
        }
    }
    if size != declared {
        return Err("part sizes changed while completing the file".into());
    }

    let id = format!("sha256:{}", hex(&hasher.finalize()));
    let destination = file_dir(&id)?;
    // Hash outside the global session lock: five concurrent 1.6 GiB files must
    // not serialise their whole-file hashing behind one mutex.
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if destination.is_dir() {
        let _ = fs::remove_dir_all(&directory);
        return file_meta(&id);
    }

    let manifest = FileManifest {
        schema: 1,
        id: id.clone(),
        size,
        name: name.unwrap_or_else(|| "file".into()),
        mime_type: mime_type.unwrap_or_else(default_mime),
        parts: selected
            .iter()
            .map(|part| FilePart {
                number: part.number,
                size: part.size,
                sha256: part.etag.clone(),
            })
            .collect(),
    };
    let manifest_path = directory.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec(&manifest).unwrap_or_default(),
    )
    .map_err(|error| format!("write {}: {}", manifest_path.display(), error))?;
    let _ = fs::remove_file(directory.join("session"));

    fs::create_dir_all(files_dir())
        .map_err(|error| format!("create {}: {}", files_dir().display(), error))?;
    match fs::rename(&directory, &destination) {
        Ok(()) => {}
        Err(_error) if destination.is_dir() => {
            let _ = fs::remove_dir_all(&directory);
            return file_meta(&id);
        }
        Err(error) => return Err(format!("rename {}: {}", destination.display(), error)),
    }
    Ok(FileMeta {
        id,
        size,
        name: manifest.name,
        mime_type: manifest.mime_type,
    })
}

#[cfg(feature = "http")]
pub fn abort_upload(id: &str, kind: UploadKind) -> Result<(), String> {
    require_upload_kind(id, kind)?;
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = upload_dir(id)?;
    if !directory.is_dir() {
        return Err("unknown upload".into());
    }
    fs::remove_dir_all(&directory)
        .map_err(|error| format!("remove {}: {}", directory.display(), error))
}

#[cfg(feature = "http")]
fn normalize_etag(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix("W/").unwrap_or(value);
    value.trim().trim_matches('"').to_string()
}

#[cfg(feature = "http")]
fn replace_file(source: &Path, destination: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        // `MoveFileEx` without replace semantics fails on Windows when the
        // destination exists; part uploads intentionally replace old parts.
        let _ = fs::remove_file(destination);
    }
    fs::rename(source, destination)
        .map_err(|error| format!("rename {}: {}", destination.display(), error))
}

#[cfg(any(feature = "http", test))]
fn next_temp() -> u64 {
    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
}

#[cfg(feature = "http")]
fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Time-derived fallback if the OS source is unavailable.
        return Ok(format!(
            "{:032x}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
    }
    Ok(hex(&bytes))
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

pub(crate) fn touch(path: &Path) {
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

    // Hash in place. The source already lives under temp_dir, so a same-volume
    // rename is enough to publish it; there is no reason to copy the bytes into
    // a second temp file.
    let mut file =
        fs::File::open(&source).map_err(|error| format!("open {}: {}", source.display(), error))?;
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
            return Err("blob is too large".into());
        }
        hasher.update(&chunk[..read]);
    }
    drop(file);
    let after = fs::metadata(&source)
        .map_err(|error| format!("stat {}: {}", source.display(), error))?
        .len();
    if after != size {
        return Err("incoming file changed while being imported".into());
    }

    let id = format!("sha256:{}", hex(&hasher.finalize()));
    let destination = blob_path(&id)?;
    if destination.is_file() {
        let _ = fs::remove_file(&source);
        return Ok(Meta { id, size });
    }
    let directory = dir();
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    match fs::rename(&source, &destination) {
        Ok(()) => {}
        Err(_error) if destination.is_file() => {
            let _ = fs::remove_file(&source);
        }
        Err(_error) => {
            // Cross-volume or a transient Windows sharing failure: fall back to
            // one copy, preserving the old behaviour.
            fs::copy(&source, &destination)
                .map_err(|error| format!("copy {}: {}", destination.display(), error))?;
            let _ = fs::remove_file(&source);
        }
    }
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
            #[cfg(feature = "http")]
            if let Some(Value::String(id)) = map.get("$file") {
                let meta = file_meta(id)?;
                let directory = file_dir(id)?;
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

fn id_hex(id: &str) -> Result<&str, String> {
    let Some(hex) = id.strip_prefix("sha256:") else {
        return Err("invalid blob id".into());
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid blob id".into());
    }
    Ok(hex)
}

fn blob_path(id: &str) -> Result<PathBuf, String> {
    Ok(dir().join(id_hex(id)?))
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{:02x}", byte);
    }
    text
}

/// One writable content-store root for every unit test that touches blobs.
/// Shared so the face download tests and the blob tests do not race by
/// pointing `SVCD_TEMP_DIR` at two different trees.
#[cfg(test)]
pub(crate) fn test_temp_dir() -> &'static Path {
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = crate::test_support::temp_dir().join(format!("svcd-blob-{}-{}", std::process::id(), nanos));
        std::env::set_var("SVCD_DATA_DIR", &root);
        std::env::set_var("SVCD_TEMP_DIR", root.join("temp"));
        std::fs::create_dir_all(root.join("temp")).expect("blob root");
        root
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn root() -> &'static Path {
        test_temp_dir()
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

    #[cfg(feature = "http")]
    #[test]
    fn part_writer_refuses_a_cross_kind_session() {
        let _ = root();
        let upload = create_upload(UploadKind::File).expect("file upload");
        let error = match PartWriter::begin(&upload, 1, UploadKind::Blob) {
            Ok(_) => panic!("blob part accepted a file session"),
            Err(error) => error,
        };
        assert!(error.contains("kind mismatch"), "{error}");
        abort_upload(&upload, UploadKind::File).expect("abort file session");
    }

    #[cfg(feature = "http")]
    #[test]
    fn cleanup_removes_a_stale_file_bundle_as_one_unit() {
        let _ = root();
        let upload = create_upload(UploadKind::File).expect("file upload");
        let mut writer = PartWriter::begin(&upload, 1, UploadKind::File).expect("part writer");
        writer.write(b"hello").expect("part write");
        writer.finish().expect("part finish");
        let meta = complete_file(&upload, None, None, None).expect("complete file");
        let directory = file_dir(&meta.id).expect("file dir");
        let manifest = directory.join("manifest.json");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&manifest)
            .expect("open manifest");
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(3 * 60 * 60)),
        )
        .expect("old manifest mtime");
        let removed = cleanup(Duration::from_secs(2 * 60 * 60)).expect("cleanup");
        assert!(removed >= 1);
        assert!(!directory.exists());
    }

    #[cfg(feature = "http")]
    #[test]
    fn file_bundles_resolve_to_a_manifest_and_size() {
        let _ = root();
        let upload = create_upload(UploadKind::File).expect("file upload");
        for (number, bytes) in [(1u32, b"hello ".as_slice()), (2u32, b"world!")] {
            let mut writer =
                PartWriter::begin(&upload, number, UploadKind::File).expect("part writer");
            writer.write(bytes).expect("part write");
            writer.finish().expect("part finish");
        }
        let meta = complete_file(
            &upload,
            None,
            Some("hello.txt".to_string()),
            Some("text/plain".to_string()),
        )
        .expect("complete file");
        assert_eq!(meta.size, 12);
        assert_eq!(
            meta.id,
            format!("sha256:{}", hex(&Sha256::digest(b"hello world!")))
        );

        let mut args = json!({"input": {"$file": meta.id}});
        resolve_args(&mut args).expect("resolve file");
        let input = &args["input"];
        assert_eq!(input["size"], 12);
        assert_eq!(input["name"], "hello.txt");
        assert_eq!(input["mimeType"], "text/plain");
        assert!(input["manifest"]
            .as_str()
            .expect("manifest")
            .ends_with("manifest.json"));
        assert!(file_dir(&meta.id).expect("file dir").is_dir());
    }
}
