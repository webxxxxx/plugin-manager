//! 分片写入：一个上传会话的分片流与已提交分片列表。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::platform::paths::Paths;

use sha2::{Digest, Sha256};

use super::path::{hex, next_temp, replace_file, touch, upload_dir};
use super::upload::{require_upload_kind, UploadKind};
use super::{Error, Result, MAX_FILE_PART_BYTES, MAX_PART_NUMBER};

/// The one bounded chunk write shared by the blob upload and a resumable part:
/// append, enforce the per-part ceiling, and report the write error against the
/// temp path.
pub(super) fn write_chunk(
    file: &mut fs::File,
    size: &mut u64,
    bytes: &[u8],
    temp: &Path,
) -> Result<()> {
    let next = size.saturating_add(bytes.len() as u64);
    if next > MAX_FILE_PART_BYTES {
        return Err(Error::too_large("blob is too large"));
    }
    file.write_all(bytes)
        .map_err(|error| Error::internal(format!("write {}: {}", temp.display(), error)))?;
    *size = next;
    Ok(())
}

/// One committed part of a resumable upload session.
#[cfg(feature = "http")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartInfo {
    pub number: u32,
    pub size: u64,
    pub etag: String,
}

#[cfg(feature = "http")]
pub(crate) fn upload_parts(paths: &Paths, id: &str, kind: UploadKind) -> Result<Vec<PartInfo>> {
    require_upload_kind(paths, id, kind)?;
    let directory = upload_dir(paths, id)?;
    if !directory.is_dir() {
        return Err(Error::not_found("unknown upload"));
    }
    let mut parts = Vec::new();
    for entry in fs::read_dir(&directory)
        .map_err(|error| Error::internal(format!("read {}: {}", directory.display(), error)))?
    {
        let entry = entry
            .map_err(|error| Error::internal(format!("read {}: {}", directory.display(), error)))?;
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
            .map_err(|error| Error::internal(format!("stat {}: {}", name, error)))?;
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
    pub fn begin(
        paths: &Paths,
        upload_id: &str,
        number: u32,
        kind: UploadKind,
    ) -> Result<PartWriter> {
        if number == 0 || number > MAX_PART_NUMBER {
            return Err(Error::invalid("part number must be between 1 and 10000"));
        }
        require_upload_kind(paths, upload_id, kind)?;
        let directory = upload_dir(paths, upload_id)?;
        if !directory.is_dir() {
            return Err(Error::not_found("unknown upload"));
        }
        // Keep the whole session alive even when an early part is older than
        // the retention window by the time the upload finishes.
        touch(&directory.join("session"));
        let temp = directory.join(format!(".part-{}-{}.tmp", number, next_temp()));
        let file = fs::File::create(&temp)
            .map_err(|error| Error::internal(format!("create {}: {}", temp.display(), error)))?;
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

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        write_chunk(&mut self.file, &mut self.size, bytes, &self.temp)?;
        self.hasher.update(bytes);
        Ok(())
    }

    pub fn finish(mut self) -> Result<PartInfo> {
        self.file
            .sync_all()
            .map_err(|error| Error::internal(format!("sync {}: {}", self.temp.display(), error)))?;
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
        fs::write(&temp_etag, etag.as_bytes()).map_err(|error| {
            Error::internal(format!("write {}: {}", temp_etag.display(), error))
        })?;
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
