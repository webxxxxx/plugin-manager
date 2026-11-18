//! 内容地址与布局：内容寻址路径、分片目录、临时文件与读取。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "http", test))]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "http")]
use std::time::UNIX_EPOCH;
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::platform::paths::Paths;

#[cfg(test)]
use super::upload::Upload;

use super::meta::Meta;
use super::{Error, Result, MAX_BLOB_BYTES};

#[cfg(any(feature = "http", test))]
static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

pub(crate) fn incoming_dir(paths: &Paths) -> PathBuf {
    paths.temp_dir.join("incoming")
}

#[cfg(test)]
pub(crate) fn put(paths: &Paths, bytes: &[u8]) -> Result<Meta> {
    let mut upload = Upload::begin(paths)?;
    upload.write(bytes)?;
    upload.finish(paths)
}

#[cfg(test)]
pub(crate) fn read(paths: &Paths, id: &str) -> Result<Vec<u8>> {
    let path = blob_path(paths, id)?;
    touch(&path);
    fs::read(&path).map_err(|error| Error::internal(format!("read {}: {}", path.display(), error)))
}

pub(crate) fn locate(paths: &Paths, id: &str) -> Result<PathBuf> {
    blob_path(paths, id)
}

pub(crate) fn path(paths: &Paths, id: &str) -> Result<PathBuf> {
    let path = locate(paths, id)?;
    touch(&path);
    Ok(path)
}

#[cfg(feature = "http")]
pub(crate) fn uploads_dir(paths: &Paths) -> PathBuf {
    paths.temp_dir.join("uploads")
}

#[cfg(feature = "http")]
pub(crate) fn valid_upload_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(feature = "http")]
pub(crate) fn upload_dir(paths: &Paths, id: &str) -> Result<PathBuf> {
    if !valid_upload_id(id) {
        return Err(Error::invalid("invalid upload id"));
    }
    Ok(uploads_dir(paths).join(id))
}

#[cfg(feature = "http")]
pub(crate) fn files_dir(paths: &Paths) -> PathBuf {
    paths.temp_dir.join("files")
}

#[cfg(feature = "http")]
pub(crate) fn file_dir(paths: &Paths, id: &str) -> Result<PathBuf> {
    Ok(files_dir(paths).join(id_hex(id)?))
}

#[cfg(feature = "http")]
pub(crate) fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // `MoveFileEx` without replace semantics fails on Windows when the
        // destination exists; part uploads intentionally replace old parts.
        let _ = fs::remove_file(destination);
    }
    fs::rename(source, destination)
        .map_err(|error| Error::internal(format!("rename {}: {}", destination.display(), error)))
}

#[cfg(any(feature = "http", test))]
pub(crate) fn next_temp() -> u64 {
    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
}

#[cfg(feature = "http")]
pub(crate) fn random_id() -> Result<String> {
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

pub(crate) fn import_file(paths: &Paths, source: &Path) -> Result<Meta> {
    let incoming = incoming_dir(paths);
    fs::create_dir_all(&incoming)
        .map_err(|error| Error::internal(format!("create {}: {}", incoming.display(), error)))?;
    let source = source
        .canonicalize()
        .map_err(|error| Error::internal(format!("open {}: {}", source.display(), error)))?;
    let incoming = incoming
        .canonicalize()
        .map_err(|error| Error::internal(format!("open {}: {}", incoming.display(), error)))?;
    if !source.starts_with(&incoming) {
        return Err(Error::invalid(
            "blob path is outside the incoming directory",
        ));
    }
    let metadata = fs::metadata(&source)
        .map_err(|error| Error::internal(format!("stat {}: {}", source.display(), error)))?;
    if metadata.len() > MAX_BLOB_BYTES {
        return Err(Error::too_large("blob is too large"));
    }

    // Hash in place. The source already lives under temp_dir, so a same-volume
    // rename is enough to publish it; there is no reason to copy the bytes into
    // a second temp file.
    let mut file = fs::File::open(&source)
        .map_err(|error| Error::internal(format!("open {}: {}", source.display(), error)))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| Error::internal(format!("read {}: {}", source.display(), error)))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        if size > MAX_BLOB_BYTES {
            return Err(Error::too_large("blob is too large"));
        }
        hasher.update(&chunk[..read]);
    }
    drop(file);
    let after = fs::metadata(&source)
        .map_err(|error| Error::internal(format!("stat {}: {}", source.display(), error)))?
        .len();
    if after != size {
        return Err(Error::invalid("incoming file changed while being imported"));
    }

    let id = format!("sha256:{}", hex(&hasher.finalize()));
    let destination = blob_path(paths, &id)?;
    if destination.is_file() {
        let _ = fs::remove_file(&source);
        return Ok(Meta { id, size });
    }
    let directory = paths.temp_dir.clone();
    fs::create_dir_all(&directory)
        .map_err(|error| Error::internal(format!("create {}: {}", directory.display(), error)))?;
    match fs::rename(&source, &destination) {
        Ok(()) => {}
        Err(_error) if destination.is_file() => {
            let _ = fs::remove_file(&source);
        }
        Err(_error) => {
            // Cross-volume or a transient Windows sharing failure: fall back to
            // one copy, preserving the old behaviour.
            fs::copy(&source, &destination).map_err(|error| {
                Error::internal(format!("copy {}: {}", destination.display(), error))
            })?;
            let _ = fs::remove_file(&source);
        }
    }
    Ok(Meta { id, size })
}

pub(crate) fn id_hex(id: &str) -> Result<&str> {
    let Some(hex) = id.strip_prefix("sha256:") else {
        return Err(Error::invalid("invalid blob id"));
    };
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::invalid("invalid blob id"));
    }
    Ok(hex)
}

pub(crate) fn blob_path(paths: &Paths, id: &str) -> Result<PathBuf> {
    blob_path_in(&paths.temp_dir, id)
}

pub(crate) fn blob_path_in(root: &Path, id: &str) -> Result<PathBuf> {
    Ok(root.join(id_hex(id)?))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(text, "{:02x}", byte);
    }
    text
}

pub(crate) fn touch(path: &Path) {
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
    }
}
