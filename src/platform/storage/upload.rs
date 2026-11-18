//! 上传会话：创建、分片提交与两种 commit 模式（内容寻址 blob / 清单文件）。

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::platform::paths::Paths;

use super::meta::{default_mime, FileManifest, FileMeta, FilePart, Meta};
use super::part::{upload_parts, PartInfo};
use super::path::{blob_path, file_dir, files_dir, hex, next_temp, random_id, upload_dir};
use super::{Error, Result, MAX_BLOB_BYTES, MAX_FILE_BYTES, MAX_FILE_PART_BYTES, MAX_PART_NUMBER};

#[cfg(feature = "http")]
static SESSION_LOCK: Mutex<()> = Mutex::new(());

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
    pub fn begin(paths: &Paths) -> Result<Upload> {
        let directory = paths.temp_dir.clone();
        fs::create_dir_all(&directory).map_err(|error| {
            Error::internal(format!("create {}: {}", directory.display(), error))
        })?;
        let temp = directory.join(format!(".upload-{}-{}", std::process::id(), next_temp()));
        let file = fs::File::create(&temp)
            .map_err(|error| Error::internal(format!("create {}: {}", temp.display(), error)))?;
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

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let size = self.size.saturating_add(bytes.len() as u64);
        if size > MAX_FILE_PART_BYTES {
            return Err(Error::too_large("blob is too large"));
        }
        self.file.write_all(bytes).map_err(|error| {
            Error::internal(format!("write {}: {}", self.temp.display(), error))
        })?;
        self.hasher.update(bytes);
        self.size = size;
        Ok(())
    }

    pub fn finish(mut self, paths: &Paths) -> Result<Meta> {
        self.file
            .sync_all()
            .map_err(|error| Error::internal(format!("sync {}: {}", self.temp.display(), error)))?;
        let digest = std::mem::replace(&mut self.hasher, Sha256::new()).finalize();
        let id = format!("sha256:{}", hex(&digest));
        let destination = blob_path(paths, &id)?;
        if destination.is_file() {
            let _ = fs::remove_file(&self.temp);
        } else if let Err(error) = fs::rename(&self.temp, &destination) {
            // A concurrent upload may have won the race; both writers produced
            // the same content, so only the loser's temp file remains.
            if destination.is_file() {
                let _ = fs::remove_file(&self.temp);
            } else {
                return Err(Error::internal(format!(
                    "rename {}: {}",
                    destination.display(),
                    error
                )));
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

/// Start a resumable upload session. The marker file keeps the directory from
/// being pruned as "empty" until the normal temp retention expires, and records
/// the commit mode so the other API cannot consume it.
#[cfg(feature = "http")]
pub(crate) fn create_upload(paths: &Paths, kind: UploadKind) -> Result<String> {
    let id = random_id()?;
    let directory = upload_dir(paths, &id)?;
    fs::create_dir_all(&directory)
        .map_err(|error| Error::internal(format!("create {}: {}", directory.display(), error)))?;
    fs::write(directory.join("session"), kind.as_str().as_bytes())
        .map_err(|error| Error::internal(format!("write session marker: {}", error)))?;
    Ok(id)
}

#[cfg(feature = "http")]
pub(crate) fn read_upload_kind(paths: &Paths, id: &str) -> Result<UploadKind> {
    let directory = upload_dir(paths, id)?;
    if !directory.is_dir() {
        return Err(Error::not_found("unknown upload"));
    }
    let marker = fs::read_to_string(directory.join("session")).unwrap_or_default();
    match marker.trim() {
        "blob" => Ok(UploadKind::Blob),
        "file" => Ok(UploadKind::File),
        // Sessions created before kind markers existed were blob sessions.
        "" => Ok(UploadKind::Blob),
        _ => Err(Error::not_found("unknown upload kind")),
    }
}

#[cfg(feature = "http")]
pub(crate) fn require_upload_kind(paths: &Paths, id: &str, expected: UploadKind) -> Result<()> {
    let actual = read_upload_kind(paths, id)?;
    if actual != expected {
        return Err(Error::conflict("upload kind mismatch"));
    }
    Ok(())
}

/// Concatenate the selected parts in ascending order into one content-addressed
/// blob. `None` selects every part that exists; an explicit list must be
/// strictly increasing and may omit parts. The session is removed on success.
#[cfg(feature = "http")]
pub(crate) fn complete_upload(
    paths: &Paths,
    id: &str,
    requested: Option<Vec<(u32, Option<String>)>>,
) -> Result<Meta> {
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = upload_dir(paths, id)?;
    if !directory.is_dir() {
        return Err(Error::not_found("unknown upload"));
    }
    let available = upload_parts(paths, id, UploadKind::Blob)?;
    let selected = select_parts(&available, requested)?;
    if selected.is_empty() {
        return Err(Error::invalid("no parts uploaded"));
    }
    let total: u64 = selected.iter().map(|part| part.size).sum();
    if total > MAX_BLOB_BYTES {
        return Err(Error::too_large("blob is too large"));
    }
    let mut upload = Upload::begin(paths)?;
    let mut buffer = [0u8; 64 * 1024];
    for part in selected {
        let path = directory.join(format!("part-{}", part.number));
        let mut file = fs::File::open(&path)
            .map_err(|error| Error::internal(format!("open {}: {}", path.display(), error)))?;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| Error::internal(format!("read {}: {}", path.display(), error)))?;
            if read == 0 {
                break;
            }
            upload.write(&buffer[..read])?;
        }
    }
    let meta = upload.finish(paths)?;
    let _ = fs::remove_dir_all(&directory);
    Ok(meta)
}

#[cfg(feature = "http")]
pub(crate) fn select_parts(
    available: &[PartInfo],
    requested: Option<Vec<(u32, Option<String>)>>,
) -> Result<Vec<PartInfo>> {
    let Some(list) = requested else {
        return Ok(available.to_vec());
    };
    if list.is_empty() {
        return Err(Error::invalid("parts is empty"));
    }
    let mut selected = Vec::new();
    let mut previous = 0u32;
    for (number, etag) in list {
        if number == 0 || number > MAX_PART_NUMBER {
            return Err(Error::invalid("part number must be between 1 and 10000"));
        }
        if number <= previous {
            return Err(Error::invalid("parts must be strictly increasing"));
        }
        let Some(part) = available.iter().find(|part| part.number == number) else {
            return Err(Error::invalid(format!("part {} is missing", number)));
        };
        if let Some(etag) = etag {
            if normalize_etag(&etag) != normalize_etag(&part.etag) {
                return Err(Error::invalid(format!(
                    "part {} etag does not match",
                    number
                )));
            }
        }
        selected.push(part.clone());
        previous = number;
    }
    Ok(selected)
}

#[cfg(feature = "http")]
pub(crate) fn file_manifest(paths: &Paths, id: &str) -> Result<(PathBuf, FileManifest)> {
    let directory = file_dir(paths, id)?;
    let path = directory.join("manifest.json");
    let bytes = fs::read(&path)
        .map_err(|error| Error::internal(format!("read {}: {}", path.display(), error)))?;
    let manifest: FileManifest = serde_json::from_slice(&bytes)
        .map_err(|error| Error::internal(format!("parse {}: {}", path.display(), error)))?;
    if manifest.id != id {
        return Err(Error::internal(format!("manifest id mismatch for {}", id)));
    }
    let size: u64 = manifest.parts.iter().map(|part| part.size).sum();
    if size != manifest.size {
        return Err(Error::internal(format!(
            "manifest size mismatch for {}",
            id
        )));
    }
    Ok((directory, manifest))
}

#[cfg(feature = "http")]
pub(crate) fn file_meta(paths: &Paths, id: &str) -> Result<FileMeta> {
    let (_, manifest) = file_manifest(paths, id)?;
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
pub(crate) fn complete_file(
    paths: &Paths,
    upload_id: &str,
    requested: Option<Vec<(u32, Option<String>)>>,
    name: Option<String>,
    mime_type: Option<String>,
) -> Result<FileMeta> {
    let directory = upload_dir(paths, upload_id)?;
    if !directory.is_dir() {
        return Err(Error::not_found("unknown upload"));
    }
    let available = upload_parts(paths, upload_id, UploadKind::File)?;
    let selected = select_parts(&available, requested)?;
    if selected.is_empty() {
        return Err(Error::invalid("no parts uploaded"));
    }
    let declared: u64 = selected.iter().map(|part| part.size).sum();
    if declared > MAX_FILE_BYTES {
        return Err(Error::too_large("file is too large"));
    }

    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    for part in &selected {
        let path = directory.join(format!("part-{}", part.number));
        let mut file = fs::File::open(&path)
            .map_err(|error| Error::internal(format!("open {}: {}", path.display(), error)))?;
        let actual = file
            .metadata()
            .map_err(|error| Error::internal(format!("stat {}: {}", path.display(), error)))?
            .len();
        if actual != part.size {
            return Err(Error::invalid(
                "part sizes changed while completing the file",
            ));
        }
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| Error::internal(format!("read {}: {}", path.display(), error)))?;
            if read == 0 {
                break;
            }
            size = size.saturating_add(read as u64);
            if size > MAX_FILE_BYTES {
                return Err(Error::too_large("file is too large"));
            }
            hasher.update(&buffer[..read]);
        }
    }
    if size != declared {
        return Err(Error::invalid(
            "part sizes changed while completing the file",
        ));
    }

    let id = format!("sha256:{}", hex(&hasher.finalize()));
    let destination = file_dir(paths, &id)?;
    // Hash outside the global session lock: five concurrent 1.6 GiB files must
    // not serialise their whole-file hashing behind one mutex.
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if destination.is_dir() {
        let _ = fs::remove_dir_all(&directory);
        return file_meta(paths, &id);
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
    .map_err(|error| Error::internal(format!("write {}: {}", manifest_path.display(), error)))?;
    let _ = fs::remove_file(directory.join("session"));

    fs::create_dir_all(files_dir(paths)).map_err(|error| {
        Error::internal(format!("create {}: {}", files_dir(paths).display(), error))
    })?;
    match fs::rename(&directory, &destination) {
        Ok(()) => {}
        Err(_error) if destination.is_dir() => {
            let _ = fs::remove_dir_all(&directory);
            return file_meta(paths, &id);
        }
        Err(error) => {
            return Err(Error::internal(format!(
                "rename {}: {}",
                destination.display(),
                error
            )))
        }
    }
    Ok(FileMeta {
        id,
        size,
        name: manifest.name,
        mime_type: manifest.mime_type,
    })
}

#[cfg(feature = "http")]
pub(crate) fn abort_upload(paths: &Paths, id: &str, kind: UploadKind) -> Result<()> {
    require_upload_kind(paths, id, kind)?;
    let _guard = SESSION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let directory = upload_dir(paths, id)?;
    if !directory.is_dir() {
        return Err(Error::not_found("unknown upload"));
    }
    fs::remove_dir_all(&directory)
        .map_err(|error| Error::internal(format!("remove {}: {}", directory.display(), error)))
}

#[cfg(feature = "http")]
pub(crate) fn normalize_etag(value: &str) -> String {
    let value = value.trim();
    let value = value.strip_prefix("W/").unwrap_or(value);
    value.trim().trim_matches('"').to_string()
}
