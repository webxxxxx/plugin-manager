//! Source scanning: every regular file, its bytes and its mode.
//!
//! The walker rejects symlinks, hard links, sparse files and unsafe paths;
//! the manifest's `files[]` and mode decisions are built from what it returns.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path};

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;

pub(super) struct SourceFile {
    pub(super) path: String,
    pub(super) data: Vec<u8>,
    pub(super) mode: u32,
}

pub(super) fn collect(root: &Path) -> Result<Vec<SourceFile>, ProblemError> {
    let mut sources = Vec::new();
    walk(root, root, &mut sources)?;
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    let mut seen = BTreeSet::new();
    for source in &sources {
        if !seen.insert(source.path.clone()) {
            return Err(Problem::Invalid.error(format!("duplicate path: {}", source.path)));
        }
    }
    Ok(sources)
}

/// Whether `st_blocks` proves that a file is sparse.
///
/// Only blocks the filesystem did allocate count as evidence: a zero block
/// count does not mean "sparse". WSL1's DrvFs keeps small files inside the
/// inode and network filesystems (9p) do not always report allocations, so a
/// plain file would look like an all-hole one. The per-file size cap in `walk`
/// already bounds what a hole can cost.
#[cfg(unix)]
pub(super) fn is_sparse(len: u64, allocated_blocks: u64) -> bool {
    allocated_blocks > 0 && allocated_blocks.saturating_mul(512) < len
}

fn walk(root: &Path, directory: &Path, out: &mut Vec<SourceFile>) -> Result<(), ProblemError> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(directory)
        .map_err(|error| {
            Problem::Internal.error(format!("read {}: {}", directory.display(), error))
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            Problem::Internal.error(format!("read {}: {}", directory.display(), error))
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = relative_path(root, &path)?;
        if relative == ".." || relative.starts_with("../") {
            return Err(Problem::Invalid.error(format!("unsafe path: {}", relative)));
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            Problem::Internal.error(format!("stat {}: {}", path.display(), error))
        })?;
        if metadata.file_type().is_symlink() {
            return Err(
                Problem::Invalid.error(format!("symbolic links are not allowed: {}", relative))
            );
        }
        if metadata.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(Problem::Invalid.error(format!("not a regular file: {}", relative)));
        }
        if matches!(relative.as_str(), "manifest.json" | "signature.json") {
            continue;
        }
        // Read at most a package's worth per file. This also rejects sparse
        // files that declare a huge logical size but hold almost no data,
        // before `fs::read` would try to materialize them.
        if metadata.len() > package::MAX_PACKAGE_BYTES {
            return Err(Problem::TooLarge.error(format!(
                "file exceeds {} MiB: {}",
                package::MAX_PACKAGE_BYTES / (1024 * 1024),
                relative
            )));
        }
        #[cfg(windows)]
        {
            let file = fs::File::open(&path).map_err(|error| {
                Problem::Internal.error(format!("read {}: {}", relative, error))
            })?;
            let info = windows_file_info(&file, &relative)?;
            if info.NumberOfLinks > 1 {
                return Err(
                    Problem::Invalid.error(format!("hard links are not allowed: {}", relative))
                );
            }
            if info.EndOfFile > info.AllocationSize {
                return Err(
                    Problem::Invalid.error(format!("sparse files are not allowed: {}", relative))
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(
                    Problem::Invalid.error(format!("hard links are not allowed: {}", relative))
                );
            }
            if is_sparse(metadata.len(), metadata.blocks()) {
                return Err(
                    Problem::Invalid.error(format!("sparse files are not allowed: {}", relative))
                );
            }
        }
        let data = fs::read(&path)
            .map_err(|error| Problem::Internal.error(format!("read {}: {}", relative, error)))?;
        out.push(SourceFile {
            path: relative,
            data,
            mode: raw_mode(&metadata, &path)?,
        });
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Result<String, ProblemError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        Problem::Invalid.error(format!("{} is outside {}", path.display(), root.display()))
    })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_str().ok_or_else(|| {
                    Problem::Invalid.error(format!("non-UTF-8 file name under {}", root.display()))
                })?;
                if name.contains('\\') {
                    return Err(Problem::Invalid
                        .error(format!("backslash in file name: {}", path.display())));
                }
                parts.push(name.to_string());
            }
            _ => return Err(Problem::Invalid.error(format!("unsafe path: {}", relative.display()))),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(windows)]
fn windows_file_info(
    file: &fs::File,
    relative: &str,
) -> Result<windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO, ProblemError> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };

    // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileStandardInfo,
            &mut info as *mut _ as *mut _,
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(Problem::Internal.error(format!(
            "stat {}: {}",
            relative,
            std::io::Error::last_os_error()
        )));
    }
    Ok(info)
}

#[cfg(unix)]
fn raw_mode(metadata: &fs::Metadata, path: &Path) -> Result<u32, ProblemError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    if mode & 0o6000 != 0 {
        return Err(
            Problem::Invalid.error(format!("setuid/setgid is not allowed: {}", path.display()))
        );
    }
    Ok(mode & 0o777)
}

#[cfg(not(unix))]
fn raw_mode(_metadata: &fs::Metadata, _path: &Path) -> Result<u32, ProblemError> {
    Ok(0)
}

pub(super) fn effective_mode(
    path: &str,
    raw: u32,
    exec: &str,
    hooks: &BTreeSet<String>,
    interpreter_runs_exec: bool,
) -> Result<u32, ProblemError> {
    #[cfg(not(unix))]
    let _ = raw;
    #[cfg(not(unix))]
    let _ = interpreter_runs_exec;
    let wants_executable_mode = path == exec || hooks.contains(path);
    if wants_executable_mode {
        #[cfg(unix)]
        {
            // Hooks are executed directly even when the service payload is run
            // through an interpreter, so only the exec path may relax the bit.
            if path == exec && interpreter_runs_exec && !hooks.contains(path) {
                return Ok(if raw & 0o111 == 0 { 0o755 } else { raw });
            }
            if raw & 0o111 == 0 {
                return Err(Problem::Invalid.error(format!("{} is not executable", path)));
            }
            Ok(raw)
        }
        #[cfg(not(unix))]
        {
            Ok(0o755)
        }
    } else {
        #[cfg(unix)]
        {
            Ok(raw)
        }
        #[cfg(not(unix))]
        {
            // Cross-packaging from Windows: the source tree has no exec bits.
            // Everything under payload/bin/ is a program by convention.
            Ok(if path.starts_with("payload/bin/") {
                0o755
            } else {
                0o644
            })
        }
    }
}

/// Whether `platform` is a target triple that is also safe to put in a file
/// name. It matches the shape `tools/check_spk.mjs` accepts (three or four
/// `[a-z0-9_]+` components) and keeps path separators and whitespace out of the
/// default output name.
pub(super) fn valid_platform(platform: &str) -> bool {
    let parts: Vec<&str> = platform.split('-').collect();
    (3..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
}

pub(super) fn normalize_exec(value: &str) -> Result<String, ProblemError> {
    let value = value.strip_prefix("./").unwrap_or(value);
    package::check_path(value)?;
    Ok(value.to_string())
}

pub(super) fn detect_exec(
    sources: &[SourceFile],
    accept_scripts: bool,
) -> Result<String, ProblemError> {
    let mut candidates: Vec<String> = sources
        .iter()
        .filter(|source| {
            is_exec_candidate_location(&source.path) && looks_executable(source, accept_scripts)
        })
        .map(|source| source.path.clone())
        .collect();
    candidates.sort();
    candidates.dedup();
    match candidates.len() {
        0 => Err(Problem::Invalid.error("cannot determine exec.path; pass --exec <path>")),
        1 => Ok(candidates.remove(0)),
        _ => Err(Problem::Invalid.error(format!(
            "multiple executable candidates: {}; pass --exec <path>",
            candidates.join(", ")
        ))),
    }
}

fn is_exec_candidate_location(path: &str) -> bool {
    match path.strip_prefix("payload/bin/") {
        Some(rest) => !rest.contains('/'),
        None => !path.contains('/'),
    }
}

fn looks_executable(source: &SourceFile, accept_scripts: bool) -> bool {
    #[cfg(unix)]
    {
        source.mode & 0o111 != 0
            || (accept_scripts
                && crate::supervise::ops::script::looks_like_script(&source.path, &source.data))
    }
    #[cfg(not(unix))]
    {
        let native = match source.path.rsplit_once('.').map(|(_, extension)| extension) {
            Some(extension) => matches!(
                extension.to_ascii_lowercase().as_str(),
                "exe" | "cmd" | "bat" | "com"
            ),
            None => false,
        };
        native
            || (accept_scripts
                && crate::supervise::ops::script::looks_like_script(&source.path, &source.data))
    }
}
