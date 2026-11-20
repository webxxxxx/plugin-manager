//! `plugin bundle`: pack a local directory into a signed, canonical `.spk`.
//!
//! The artifact defaults to `<name>-<version>-<platform>.spk`: the whole target
//! triple, because the architecture prefix alone cannot distinguish musl from
//! gnu (or other ABIs sharing an arch).
//!
//! The directory is the source of truth for `files[]`: every regular file is
//! hashed, sized and mode-checked here, then the manifest is serialized with
//! sorted keys and no trailing newline before it is signed. An optional
//! `manifest.json` supplies author fields such as capabilities and arguments;
//! `files[]` is always recomputed.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Map, Value};

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage::Manifest;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub dir: PathBuf,
    pub out: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub key_id: Option<String>,
    pub id: Option<String>,
    pub version: Option<String>,
    pub exec: Option<String>,
    pub desc: Option<String>,
    /// Emit schema-2 lifecycle actions from the exec script's declarations.
    pub from_core: bool,
    /// Host interpreter required by a script payload.
    pub interpreter: Option<String>,
    pub interpreter_version: Option<String>,
    pub interpreter_args: Vec<String>,
    /// Device compatibility ids (`--compat sr6110`); empty means universal.
    pub compat: Vec<String>,
    /// Target triple written into the manifest. Defaults to the host triple;
    /// cross-packaging for a device (`aarch64-unknown-linux-musl`) sets it.
    pub platform: Option<String>,
    /// Produce an unsigned package. Local development only: it installs on a
    /// host with `allow_insecure_install`, and `plugin verify` reports the
    /// signature as `missing`.
    pub unsigned: bool,
}

pub(super) struct SourceFile {
    path: String,
    data: Vec<u8>,
    mode: u32,
}

pub fn bundle(options: &Options) -> Result<PathBuf, ProblemError> {
    let dir = fs::canonicalize(&options.dir).map_err(|error| {
        Problem::Invalid.error(format!("cannot read {}: {}", options.dir.display(), error))
    })?;
    if !dir.is_dir() {
        return Err(Problem::Invalid.error(format!("not a directory: {}", dir.display())));
    }
    if options.unsigned && (options.key.is_some() || options.key_id.is_some()) {
        return Err(Problem::Invalid.error("--unsigned cannot be combined with --key or --key-id"));
    }
    let key_path = match (options.key.as_deref(), options.unsigned) {
        (_, true) => None,
        (Some(key), false) => {
            let key = fs::canonicalize(key).map_err(|error| {
                Problem::Internal.error(format!("read key {}: {}", key.display(), error))
            })?;
            if key.starts_with(&dir) {
                return Err(Problem::Invalid
                    .error("the signing key must not be inside the bundle directory"));
            }
            Some(key)
        }
        (None, false) => {
            return Err(Problem::Invalid.error(
                "bundle requires --key <ed25519-private.pem> (or --unsigned for local builds)",
            ))
        }
    };
    if let Some(platform) = &options.platform {
        if !valid_platform(platform) {
            return Err(Problem::UnsupportedPlatform
                .error(format!("invalid target triple: {:?}", platform)));
        }
    }

    let mut sources = collect(&dir)?;
    if sources.is_empty() {
        return Err(Problem::Invalid.error("directory contains no files to bundle"));
    }
    if sources.len() > package::MAX_FILES {
        return Err(Problem::Invalid.error(format!(
            "too many files to bundle (limit {})",
            package::MAX_FILES
        )));
    }
    let total: u64 = sources.iter().map(|source| source.data.len() as u64).sum();
    if total > package::MAX_PACKAGE_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "package exceeds {} MiB",
            package::MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }

    let skeleton = read_skeleton(&dir)?;
    let (manifest_bytes, parsed) =
        assemble_manifest(&dir, &mut sources, skeleton.unwrap_or_default(), options)?;

    // The filename carries the whole target triple: `x86_64` alone is shared by
    // the musl and gnu packages, so an arch-only name cannot identify which
    // host may install the file.
    let path = options.out.clone().unwrap_or_else(|| {
        PathBuf::from(format!(
            "{}-{}-{}.spk",
            parsed.name, parsed.version, parsed.platform
        ))
    });
    let absolute = absolute_output(&path)?;
    if absolute.starts_with(&dir) {
        return Err(Problem::Invalid.error("package output must be outside the bundle directory"));
    }

    let signature_bytes = match &key_path {
        None => None,
        Some(key_path) => {
            let key_id = match &options.key_id {
                Some(key_id) => key_id.clone(),
                None => default_key_id(key_path)?,
            };
            if !package::valid_key_id(&key_id) {
                return Err(
                    Problem::Invalid.error(format!("invalid keyId: {} (pass --key-id)", key_id))
                );
            }
            let signing_key = read_private_key(key_path)?;
            let signature = signing_key.sign(&manifest_bytes);
            let signature = serde_json::json!({
                "alg": "ed25519",
                "keyId": key_id,
                "sig": BASE64.encode(signature.to_bytes()),
            });
            Some(serde_json::to_vec(&signature).map_err(|error| {
                Problem::Invalid.error(format!("serialize signature: {}", error))
            })?)
        }
    };

    let mut entries: Vec<(&str, &[u8], u32)> = vec![("manifest.json", &manifest_bytes, 0o644)];
    if let Some(signature_bytes) = &signature_bytes {
        entries.push(("signature.json", signature_bytes.as_slice(), 0o644));
    }
    for source in &sources {
        entries.push((source.path.as_str(), source.data.as_slice(), source.mode));
    }
    let archive = package::write_tar(&entries)?;
    if archive.len() as u64 > package::MAX_PACKAGE_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "package exceeds {} MiB",
            package::MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }

    fs::write(&path, &archive)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;

    Ok(path)
}

pub(super) fn assemble_manifest(
    dir: &Path,
    sources: &mut [SourceFile],
    mut manifest: Map<String, Value>,
    options: &Options,
) -> Result<(Vec<u8>, Manifest), ProblemError> {
    let directory_name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let name = match &options.id {
        Some(id) => id.clone(),
        None => manifest
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| directory_name.clone())
            .ok_or_else(|| {
                Problem::Invalid
                    .error("cannot derive the plugin id from the directory name; pass --id")
            })?,
    };
    if !package::valid_name(&name) {
        return Err(Problem::Invalid.error(format!(
            "invalid plugin id: {} (expected [a-z0-9][a-z0-9_-]{{0,63}}; pass --id)",
            name
        )));
    }
    let version = match &options.version {
        Some(version) => version.clone(),
        None => manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "0.1.0".to_string()),
    };
    if !package::valid_version(&version) {
        return Err(Problem::Invalid.error(format!(
            "version is not semver: {} (pass --version)",
            version
        )));
    }
    let desc = match &options.desc {
        Some(desc) => desc.clone(),
        None => manifest
            .get("desc")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| directory_name.clone())
            .unwrap_or_else(|| name.clone()),
    };
    if desc.is_empty() {
        return Err(Problem::Invalid.error("desc must not be empty (pass --desc)"));
    }

    let declared_exec = manifest
        .get("exec")
        .and_then(Value::as_object)
        .and_then(|exec| exec.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let exec_path = match &options.exec {
        Some(value) => normalize_exec(value)?,
        None => match declared_exec {
            Some(path) => path,
            None => detect_exec(sources, options.from_core || options.interpreter.is_some())?,
        },
    };
    if !sources.iter().any(|source| source.path == exec_path) {
        return Err(Problem::Invalid.error(format!("exec.path is not in files[]: {}", exec_path)));
    }
    package::check_path(&exec_path)?;

    if options.from_core {
        let source = sources
            .iter()
            .find(|source| source.path == exec_path)
            .expect("exec membership checked above");
        if package::is_native_binary(&source.data) {
            return Err(Problem::Invalid
                .error("--from-core needs a script payload, but exec is a native binary"));
        }
        if manifest.contains_key("capabilities") {
            return Err(Problem::Invalid.error("--from-core packages cannot declare capabilities"));
        }
        let extension = std::path::Path::new(&exec_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let actions = crate::supervise::ops::script::declared_actions(
            &String::from_utf8_lossy(&source.data),
            &extension,
        );
        if actions.any() {
            let mut lifecycle = Map::new();
            if actions.start {
                lifecycle.insert(
                    "start".into(),
                    serde_json::json!({"path": exec_path, "args": ["start"]}),
                );
            }
            if actions.stop {
                lifecycle.insert(
                    "stop".into(),
                    serde_json::json!({"path": exec_path, "args": ["stop"]}),
                );
            }
            if actions.status {
                lifecycle.insert(
                    "status".into(),
                    serde_json::json!({"path": exec_path, "args": ["status"]}),
                );
            }
            manifest.insert("lifecycle".into(), Value::Object(lifecycle));
        } else {
            manifest.remove("lifecycle");
        }
        let mut exec = manifest
            .get("exec")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        exec.insert("ready".into(), Value::String("exec".into()));
        manifest.insert("exec".into(), Value::Object(exec));
    }

    if options.interpreter_version.is_some() && options.interpreter.is_none() {
        return Err(Problem::Invalid.error("--interpreter-version requires --interpreter"));
    }
    if !options.interpreter_args.is_empty() && options.interpreter.is_none() {
        return Err(Problem::Invalid.error("--interpreter-arg requires --interpreter"));
    }
    if let Some(name) = &options.interpreter {
        let mut interpreter = manifest
            .get("interpreter")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        interpreter.insert("name".into(), Value::String(name.clone()));
        if let Some(version) = &options.interpreter_version {
            interpreter.insert("version".into(), Value::String(version.clone()));
        }
        if !options.interpreter_args.is_empty() {
            interpreter.insert(
                "args".into(),
                Value::Array(
                    options
                        .interpreter_args
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        manifest.insert("interpreter".into(), Value::Object(interpreter));
    }
    if options.from_core && !manifest.contains_key("interpreter") {
        return Err(Problem::Invalid
            .error("--from-core with a script payload requires --interpreter <name>"));
    }

    if !manifest.contains_key("hooks") {
        let mut hooks = Map::new();
        if sources
            .iter()
            .any(|source| source.path == "hooks/post-install")
        {
            hooks.insert(
                "postInstall".into(),
                Value::String("hooks/post-install".into()),
            );
        }
        if sources
            .iter()
            .any(|source| source.path == "hooks/pre-remove")
        {
            hooks.insert("preRemove".into(), Value::String("hooks/pre-remove".into()));
        }
        if !hooks.is_empty() {
            manifest.insert("hooks".into(), Value::Object(hooks));
        }
    }
    let hook_paths: BTreeSet<String> = manifest
        .get("hooks")
        .and_then(Value::as_object)
        .map(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    #[cfg(unix)]
    for path in &hook_paths {
        if let Some(source) = sources.iter().find(|source| &source.path == path) {
            if !source.data.starts_with(b"#!") {
                return Err(
                    Problem::Invalid.error(format!("hook must start with a shebang: {}", path))
                );
            }
            if source.data.contains(&b'\r') {
                return Err(
                    Problem::Invalid.error(format!("hook must use LF line endings: {}", path))
                );
            }
        }
    }
    let interpreter_runs_exec = manifest.contains_key("interpreter");
    for source in sources.iter_mut() {
        source.mode = effective_mode(
            &source.path,
            source.mode,
            &exec_path,
            &hook_paths,
            interpreter_runs_exec,
        )?;
    }

    let mut files = Vec::with_capacity(sources.len());
    for source in sources.iter() {
        files.push(serde_json::json!({
            "path": source.path,
            "size": source.data.len(),
            "sha256": package::hex_sha256(&source.data),
            "mode": source.mode,
        }));
    }

    // Device compatibility: CLI wins; an authored manifest may still provide it.
    if !options.compat.is_empty() {
        let mut seen = BTreeSet::new();
        let mut devices = Vec::new();
        for device in &options.compat {
            crate::platform::compat::validate_device_id(device).map_err(|error| {
                Problem::Invalid.error(format!("--compat {}: {}", device, error))
            })?;
            if seen.insert(device.clone()) {
                devices.push(device.clone());
            }
        }
        manifest.insert("compat".into(), serde_json::json!({ "devices": devices }));
    }
    let has_kernel_module = sources
        .iter()
        .any(|source| source.path.to_ascii_lowercase().ends_with(".ko"));
    let declared_compat = manifest
        .get("compat")
        .and_then(|value| value.get("devices"))
        .and_then(Value::as_array)
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    if has_kernel_module && !declared_compat {
        return Err(Problem::Invalid.error(
            "the payload contains a kernel module (.ko); pass --compat <device> so it is only \
             installed on the matching firmware generation",
        ));
    }

    let needs_three = declared_compat || has_kernel_module;
    let needs_two = manifest.contains_key("lifecycle") || manifest.contains_key("interpreter");
    let computed_schema = if needs_three {
        3
    } else if needs_two {
        2
    } else {
        1
    };
    let authored_schema = manifest.get("schema").and_then(Value::as_u64).unwrap_or(0) as u32;
    manifest.insert(
        "schema".into(),
        Value::from(computed_schema.max(authored_schema)),
    );
    manifest.entry("protocol").or_insert_with(|| Value::from(1));
    manifest.insert("name".into(), Value::String(name));
    manifest.insert("version".into(), Value::String(version));
    manifest.insert("desc".into(), Value::String(desc));
    manifest.insert(
        "platform".into(),
        Value::String(
            options
                .platform
                .clone()
                .unwrap_or_else(|| crate::platform::target().to_string()),
        ),
    );
    let mut exec = manifest
        .get("exec")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    exec.insert("path".into(), Value::String(exec_path));
    manifest.insert("exec".into(), Value::Object(exec));
    manifest.insert("files".into(), Value::Array(files));

    if !manifest.contains_key("config")
        && sources.iter().any(|source| source.path == "config/config")
    {
        manifest.insert("config".into(), Value::String("config/config".into()));
    }
    if !manifest.contains_key("ui")
        && sources
            .iter()
            .any(|source| source.path.starts_with("payload/web/"))
    {
        manifest.insert("ui".into(), serde_json::json!({"type": "web"}));
    }
    let manifest_bytes = serde_json::to_vec(&Value::Object(manifest))
        .map_err(|error| Problem::Invalid.error(format!("serialize manifest: {}", error)))?;
    let parsed = package::parse_manifest(&manifest_bytes)?;
    package::validate_manifest(&parsed)?;
    let borrowed: Vec<(&str, &[u8])> = sources
        .iter()
        .map(|source| (source.path.as_str(), source.data.as_slice()))
        .collect();
    package::validate_script_payload(&parsed, &borrowed)?;
    Ok((manifest_bytes, parsed))
}

fn absolute_output(path: &Path) -> Result<PathBuf, ProblemError> {
    let absolute = std::path::absolute(path).map_err(|error| {
        Problem::Invalid.error(format!("resolve {}: {}", path.display(), error))
    })?;
    let parent = match absolute.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent = fs::canonicalize(parent).map_err(|error| {
        Problem::Invalid.error(format!("resolve {}: {}", path.display(), error))
    })?;
    Ok(match absolute.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    })
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
            return Ok(raw);
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

pub(super) fn detect_exec(sources: &[SourceFile], accept_scripts: bool) -> Result<String, ProblemError> {
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

pub(super) fn read_skeleton(dir: &Path) -> Result<Option<Map<String, Value>>, ProblemError> {
    let path = dir.join("manifest.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Problem::Internal.error(format!("stat {}: {}", path.display(), error)))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(Problem::Invalid.error("symbolic links are not allowed: manifest.json"));
    }
    if !metadata.is_file() {
        return Err(Problem::Invalid.error("manifest.json must be a regular file"));
    }
    let bytes = fs::read(&path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| Problem::Invalid.error(format!("{}: {}", path.display(), error)))?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(Problem::Invalid.error("manifest.json must contain a JSON object")),
    }
}

pub(super) fn default_key_id(path: &Path) -> Result<String, ProblemError> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Problem::Invalid.error("cannot derive keyId from the key file name; pass --key-id")
        })
}

pub(super) fn read_private_key(path: &Path) -> Result<SigningKey, ProblemError> {
    let bytes = fs::read(path).map_err(|error| {
        Problem::Internal.error(format!("read key {}: {}", path.display(), error))
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        Problem::Invalid.error(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        ))
    })?;
    let body = pem_body(&text, "PRIVATE KEY").ok_or_else(|| {
        Problem::Invalid.error(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        ))
    })?;
    let body: String = body.lines().map(str::trim).collect();
    let der = BASE64.decode(body).map_err(|error| {
        Problem::Invalid.error(format!("key {} base64: {}", path.display(), error))
    })?;
    const PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    if der.len() != 48 || !der.starts_with(&PREFIX) {
        return Err(Problem::Invalid.error(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        )));
    }
    let seed: [u8; 32] = der[16..]
        .try_into()
        .map_err(|_| Problem::Invalid.error("Ed25519 private key length"))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn pem_body<'a>(text: &'a str, label: &str) -> Option<&'a str> {
    let begin = format!("-----BEGIN {}-----", label);
    let end = format!("-----END {}-----", label);
    let start = text.find(&begin)? + begin.len();
    let stop = text[start..].find(&end)? + start;
    Some(text[start..stop].trim())
}

#[cfg(test)]
mod tests;
