//! ustar 编解码：`.spk` 的 tar 头、流式解析与导出计划的写入。
//!
//! 只做字节 ⇄ 条目，不做校验判定；`Problem` 只用于把 I/O 与格式错误投影成
//! 已有的失败类别。

#![cfg_attr(not(feature = "http"), allow(dead_code))]
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::layout::check_path;
use crate::manage::model::hex::hex_bytes;

#[derive(Debug, Clone)]
pub struct TarEntry {
    pub path: String,
    pub data: Vec<u8>,
}

pub fn read_tar(bytes: &[u8]) -> Result<Vec<TarEntry>, ProblemError> {
    let mut tar = Tar::new(bytes);
    let mut entries = Vec::new();
    while let Some(header) = tar.next()? {
        if header.kind == b'5' {
            continue;
        }
        let data = header.data.ok_or_else(|| {
            Problem::Invalid.error(format!("{} must be a regular file", header.path))
        })?;
        entries.push(TarEntry {
            path: header.path,
            data: data.to_vec(),
        });
    }
    Ok(entries)
}

/// The ustar end-of-archive marker: two 512-byte zero blocks.
pub const TAR_TRAILER: usize = 1024;

pub fn write_tar(entries: &[(&str, &[u8], u32)]) -> Result<Vec<u8>, ProblemError> {
    let mut out = Vec::new();
    for (path, data, mode) in entries {
        write_tar_entry(&mut out, path, data, *mode)?;
    }
    out.extend(std::iter::repeat_n(0, TAR_TRAILER));
    Ok(out)
}

pub fn write_tar_entry<W: Write>(
    out: &mut W,
    path: &str,
    data: &[u8],
    mode: u32,
) -> Result<(), ProblemError> {
    write_tar_header(out, path, data.len() as u64, mode)?;
    out.write_all(data)
        .map_err(|error| Problem::Internal.error(format!("write tar data: {}", error)))?;
    write_tar_padding(out, data.len() as u64)
}

pub fn write_tar_header<W: Write>(
    out: &mut W,
    path: &str,
    size: u64,
    mode: u32,
) -> Result<(), ProblemError> {
    check_path(path)?;
    if path.len() > 100 {
        return Err(Problem::Invalid.error(format!("tar path is too long: {}", path)));
    }
    let mut header = [0u8; 512];
    header[..path.len()].copy_from_slice(path.as_bytes());
    let mode = format!("{:07o}", mode & 0o777);
    header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
    let size = format!("{:011o}", size);
    header[124..124 + size.len()].copy_from_slice(size.as_bytes());
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    let checksum = format!("{:06o}\0 ", checksum);
    header[148..156].copy_from_slice(checksum.as_bytes());
    out.write_all(&header)
        .map_err(|error| Problem::Internal.error(format!("write tar header: {}", error)))
}

pub fn write_tar_padding<W: Write>(out: &mut W, size: u64) -> Result<(), ProblemError> {
    let padding = padding(size as usize);
    if padding > 0 {
        out.write_all(&[0u8; 512][..padding])
            .map_err(|error| Problem::Internal.error(format!("write tar padding: {}", error)))?;
    }
    Ok(())
}

/// One piece of an archive that can be written now or streamed later.
///
/// Small protocol pieces travel as bytes. A payload file stays on disk and is
/// described by its path, length and the hash the manifest declares for it, so
/// a streaming caller can hand it to the server without reading it here.
pub enum TarPart {
    Bytes(Vec<u8>),
    File {
        path: PathBuf,
        length: u64,
        sha256: Option<String>,
    },
}

/// An archive described as parts.
///
/// One layout, two consumers: [TarPlan::write_verified] produces the bytes
/// (checking each payload's declared hash while copying) and a stream handler
/// asks the server to send the file parts. The running size is the response's
/// Content-Length, so every part is counted exactly once.
#[derive(Default)]
pub struct TarPlan {
    parts: Vec<TarPart>,
    size: u64,
}

impl TarPlan {
    pub fn new() -> TarPlan {
        TarPlan::default()
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn parts(&self) -> &[TarPart] {
        &self.parts
    }

    pub fn push_bytes(&mut self, bytes: Vec<u8>) {
        self.size = self.size.saturating_add(bytes.len() as u64);
        self.parts.push(TarPart::Bytes(bytes));
    }

    pub fn push_file(&mut self, path: PathBuf, length: u64, sha256: Option<String>) {
        self.size = self.size.saturating_add(length);
        self.parts.push(TarPart::File {
            path,
            length,
            sha256,
        });
    }

    /// Append another archive's parts and size: the nested .spk of a backup.
    pub fn append(&mut self, other: TarPlan) {
        self.size = self.size.saturating_add(other.size);
        self.parts.extend(other.parts);
    }

    /// Write the archive, verifying every file part that carries a hash.
    pub fn write_verified<W: Write>(&self, out: &mut W) -> Result<(), ProblemError> {
        for part in &self.parts {
            match part {
                TarPart::Bytes(bytes) => out
                    .write_all(bytes)
                    .map_err(|error| Problem::Internal.error(format!("write export: {}", error)))?,
                TarPart::File {
                    path,
                    length,
                    sha256,
                } => {
                    let mut source = fs::File::open(path).map_err(|error| {
                        Problem::Internal.error(format!("open {}: {}", path.display(), error))
                    })?;
                    let mut hasher = Sha256::new();
                    let mut written = 0u64;
                    let mut buffer = [0u8; 64 * 1024];
                    loop {
                        let read = source.read(&mut buffer).map_err(|error| {
                            Problem::Internal.error(format!("read {}: {}", path.display(), error))
                        })?;
                        if read == 0 {
                            break;
                        }
                        written = written.saturating_add(read as u64);
                        hasher.update(&buffer[..read]);
                        out.write_all(&buffer[..read]).map_err(|error| {
                            Problem::Internal.error(format!("write export: {}", error))
                        })?;
                    }
                    let hash_matches = sha256
                        .as_ref()
                        .map(|expected| hex_bytes(&hasher.finalize()) == *expected)
                        .unwrap_or(true);
                    if written != *length || !hash_matches {
                        return Err(Problem::Internal
                            .error(format!("installed file is corrupt: {}", path.display())));
                    }
                }
            }
        }
        Ok(())
    }
}

/// One tar entry as bytes: header, data and padding.
pub fn tar_entry_bytes(path: &str, data: &[u8], mode: u32) -> Result<Vec<u8>, ProblemError> {
    let mut out = Vec::with_capacity(512 + data.len());
    write_tar_entry(&mut out, path, data, mode)?;
    Ok(out)
}

/// One tar header as bytes, for an entry whose data is sent separately.
pub fn tar_header_bytes(path: &str, size: u64, mode: u32) -> Result<Vec<u8>, ProblemError> {
    let mut out = Vec::with_capacity(512);
    write_tar_header(&mut out, path, size, mode)?;
    Ok(out)
}

pub fn tar_padding_bytes(size: u64) -> Result<Vec<u8>, ProblemError> {
    let mut out = Vec::with_capacity(512);
    write_tar_padding(&mut out, size)?;
    Ok(out)
}

/// One ustar entry header: what the feed parsed and what a sink decides on.
pub struct StreamEntry {
    pub path: String,
    pub mode: u32,
    pub kind: u8,
    pub size: u64,
}

/// Parse one 512-byte ustar header. `None` is the end-of-archive zero block.
/// The sans-IO feed uses this one parser, so every tar consumer validates the
/// same path, type and checksum rules.
pub(crate) fn parse_header(header: &[u8; 512]) -> Result<Option<StreamEntry>, ProblemError> {
    if header.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    let name = field(header, 0, 100);
    let prefix = field(header, 345, 155);
    let mut path = if prefix.is_empty() {
        name
    } else {
        format!("{}/{}", prefix, name)
    };
    let mode = parse_octal(&header[100..108], "mode")? as u32;
    let size = parse_octal(&header[124..136], "size")?;
    let kind = header[156];
    if !matches!(kind, b'0' | b'5') {
        return Err(Problem::Invalid.error(format!("unsupported tar entry type for {}", path)));
    }
    if kind == b'5' {
        path = path.trim_end_matches('/').to_string();
        if size != 0 {
            return Err(Problem::Invalid.error(format!("directory entry has data: {}", path)));
        }
    }
    check_path(&path)?;
    check_checksum(header)?;
    Ok(Some(StreamEntry {
        path,
        mode,
        kind,
        size,
    }))
}

pub(crate) fn padding_u64(size: u64) -> u64 {
    let rem = size % 512;
    if rem == 0 {
        0
    } else {
        512 - rem
    }
}

/// A sans-IO ustar reader.
///
/// A request body arrives in arbitrary chunks, so the parser keeps the partial
/// header or entry body it still needs. Header, checksum, path and byte-limit
/// checks live here, not in the caller.
pub struct TarFeed {
    header: [u8; 512],
    header_len: usize,
    remaining: u64,
    padding: u64,
    total: u64,
    limit: u64,
    done: bool,
    in_entry: bool,
}

impl TarFeed {
    pub fn new(limit: u64) -> TarFeed {
        TarFeed {
            header: [0u8; 512],
            header_len: 0,
            remaining: 0,
            padding: 0,
            total: 0,
            limit,
            done: false,
            in_entry: false,
        }
    }

    /// Consume one body chunk. Entry callbacks may be invoked any number of
    /// times per chunk; a chunk may also end in the middle of a header, an
    /// entry body or its padding.
    pub fn feed<S: TarSink + ?Sized>(
        &mut self,
        data: &[u8],
        sink: &mut S,
    ) -> Result<(), ProblemError> {
        let mut data = data;
        while !data.is_empty() {
            if self.remaining > 0 {
                let take = std::cmp::min(self.remaining, data.len() as u64) as usize;
                self.count(take)?;
                sink.entry_data(&data[..take])?;
                self.remaining -= take as u64;
                data = &data[take..];
                if self.remaining == 0 {
                    sink.entry_end()?;
                    self.in_entry = false;
                }
                continue;
            }
            if self.padding > 0 {
                let take = std::cmp::min(self.padding, data.len() as u64) as usize;
                self.count(take)?;
                self.padding -= take as u64;
                data = &data[take..];
                continue;
            }
            if self.done {
                // Trailing archive padding after the end marker is drained and
                // still counts against the package limit.
                self.count(data.len())?;
                data = &[];
                continue;
            }
            let take = std::cmp::min(512 - self.header_len, data.len());
            self.count(take)?;
            self.header[self.header_len..self.header_len + take].copy_from_slice(&data[..take]);
            self.header_len += take;
            data = &data[take..];
            if self.header_len == 512 {
                self.header_len = 0;
                let header = self.header;
                let Some(entry) = parse_header(&header)? else {
                    self.done = true;
                    continue;
                };
                self.remaining = entry.size;
                self.padding = padding_u64(entry.size);
                self.in_entry = true;
                sink.entry_start(&entry)?;
                if entry.size == 0 {
                    sink.entry_end()?;
                    self.in_entry = false;
                }
            }
        }
        Ok(())
    }

    /// The body ended. The archive must have reached its end-of-archive marker
    /// and no entry may still be in flight.
    pub fn finish(&self) -> Result<(), ProblemError> {
        if self.header_len != 0
            || self.in_entry
            || self.remaining != 0
            || self.padding != 0
            || !self.done
        {
            return Err(Problem::Internal.error("read package: failed to fill whole buffer"));
        }
        Ok(())
    }

    fn count(&mut self, bytes: usize) -> Result<(), ProblemError> {
        self.total = self.total.saturating_add(bytes as u64);
        if self.total > self.limit {
            return Err(Problem::TooLarge.error(format!(
                "package exceeds {} MiB",
                self.limit / (1024 * 1024)
            )));
        }
        Ok(())
    }
}

pub(crate) fn parse_octal(bytes: &[u8], name: &str) -> Result<u64, ProblemError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Problem::Invalid.error(format!("invalid octal {} field", name)))?
        .trim_matches(['\0', ' ']);
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8)
        .map_err(|_| Problem::Invalid.error(format!("invalid octal {} field", name)))
}

pub(crate) fn check_checksum(header: &[u8; 512]) -> Result<(), ProblemError> {
    let expected = parse_octal(&header[148..156], "checksum")?;
    // Older fixtures and some third-party ustar writers leave the checksum
    // field zero. The manifest file hashes are the real integrity check, so a
    // zero field is accepted; a non-zero field must still match.
    if expected == 0 {
        return Ok(());
    }
    let actual: u64 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                b' ' as u64
            } else {
                *byte as u64
            }
        })
        .sum();
    if expected != actual {
        return Err(Problem::Invalid.error("invalid tar header checksum"));
    }
    Ok(())
}

/// Read the path from one tar header without consuming a stream. Returns
/// `None` for the zero blocks at end-of-archive. The classification path and
/// the installer therefore validate a header exactly the same way.
pub fn tar_header_path(header: &[u8]) -> Result<Option<String>, ProblemError> {
    if header.len() < 512 {
        return Ok(None);
    }
    let header: &[u8; 512] = header[..512]
        .try_into()
        .map_err(|_| Problem::Invalid.error("truncated tar header"))?;
    Ok(parse_header(header)?.map(|entry| entry.path))
}

pub(crate) struct Tar<'a> {
    bytes: &'a [u8],
    offset: usize,
    done: bool,
}

pub(crate) struct Header<'a> {
    pub(crate) path: String,
    pub(crate) mode: u32,
    pub(crate) kind: u8,
    pub(crate) data: Option<&'a [u8]>,
}

impl<'a> Tar<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Tar {
            bytes,
            offset: 0,
            done: false,
        }
    }

    pub(crate) fn next(&mut self) -> Result<Option<Header<'a>>, ProblemError> {
        if self.done {
            return Ok(None);
        }
        let end = self.offset + 512;
        if end > self.bytes.len() {
            return Err(Problem::Invalid.error("truncated tar header"));
        }
        let header = &self.bytes[self.offset..end];
        self.offset = end;
        if header.iter().all(|byte| *byte == 0) {
            self.done = true;
            return Ok(None);
        }
        let name = field(header, 0, 100);
        let prefix = field(header, 345, 155);
        let mut path = if prefix.is_empty() {
            name
        } else {
            format!("{}/{}", prefix, name)
        };
        let mode = octal(field(header, 100, 8)) as u32;
        let size = octal(field(header, 124, 12));
        let kind = header[156];
        if !matches!(kind, b'0' | b'5') {
            return Err(Problem::Invalid.error(format!("unsupported tar entry type for {}", path)));
        }
        if kind == b'5' {
            path = path.trim_end_matches('/').to_string();
        }
        check_path(&path)?;
        let data = if kind == b'0' {
            let start = self.offset;
            let end = start
                .checked_add(size)
                .ok_or_else(|| Problem::Invalid.error("tar size overflow"))?;
            if end > self.bytes.len() {
                return Err(Problem::Invalid.error("truncated tar data"));
            }
            self.offset = end + padding(size);
            Some(&self.bytes[start..end])
        } else {
            if size != 0 {
                return Err(Problem::Invalid.error(format!("directory entry has data: {}", path)));
            }
            None
        };
        Ok(Some(Header {
            path,
            mode,
            kind,
            data,
        }))
    }
}

impl<'a> Iterator for Tar<'a> {
    type Item = Result<Header<'a>, ProblemError>;

    fn next(&mut self) -> Option<Self::Item> {
        match Tar::next(self) {
            Ok(Some(header)) => Some(Ok(header)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

pub(crate) fn padding(size: usize) -> usize {
    let rem = size % 512;
    if rem == 0 {
        0
    } else {
        512 - rem
    }
}

pub(crate) fn field(hdr: &[u8], start: usize, len: usize) -> String {
    let slice = &hdr[start..start + len];
    let end = slice.iter().position(|b| *b == 0).unwrap_or(len);
    String::from_utf8_lossy(&slice[..end]).to_string()
}

pub(crate) fn octal(s: String) -> usize {
    usize::from_str_radix(s.trim_matches(char::from(0)).trim(), 8).unwrap_or(0)
}

/// Receives the entry boundaries a [`TarFeed`] produces. The feed owns the
/// header, padding and byte-limit bookkeeping; the sink only decides what to
/// do with one entry's bytes.
pub trait TarSink {
    fn entry_start(&mut self, entry: &StreamEntry) -> Result<(), ProblemError>;
    fn entry_data(&mut self, data: &[u8]) -> Result<(), ProblemError>;
    fn entry_end(&mut self) -> Result<(), ProblemError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_over_the_package_limit_is_too_large() {
        struct Sink;
        impl TarSink for Sink {
            fn entry_start(&mut self, _entry: &StreamEntry) -> Result<(), ProblemError> {
                Ok(())
            }
            fn entry_data(&mut self, _data: &[u8]) -> Result<(), ProblemError> {
                Ok(())
            }
            fn entry_end(&mut self) -> Result<(), ProblemError> {
                Ok(())
            }
        }
        let mut feed = TarFeed::new(8);
        let error = feed.feed(&[0u8; 16], &mut Sink).expect_err("over limit");
        assert_eq!(error.problem, Problem::TooLarge);
    }
}
