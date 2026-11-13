//! `plugin bundle`: pack a local directory into a signed, canonical `.spk`.
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

use crate::package::{self, Manifest};

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
}

struct SourceFile {
    path: String,
    data: Vec<u8>,
    mode: u32,
}

pub fn bundle(options: &Options) -> Result<PathBuf, String> {
    let key_path = options.key.as_deref().ok_or_else(|| {
        "bundle requires --key <ed25519-private.pem>; refusing to produce an unsigned package"
            .to_string()
    })?;
    let key_path = fs::canonicalize(key_path)
        .map_err(|error| format!("read key {}: {}", key_path.display(), error))?;
    let dir = fs::canonicalize(&options.dir)
        .map_err(|error| format!("cannot read {}: {}", options.dir.display(), error))?;
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    if key_path.starts_with(&dir) {
        return Err("the signing key must not be inside the bundle directory".into());
    }

    let mut sources = collect(&dir)?;
    if sources.is_empty() {
        return Err("directory contains no files to bundle".into());
    }
    if sources.len() > package::MAX_FILES {
        return Err(format!(
            "too many files to bundle (limit {})",
            package::MAX_FILES
        ));
    }
    let total: u64 = sources.iter().map(|source| source.data.len() as u64).sum();
    if total > package::MAX_PACKAGE_BYTES {
        return Err(format!(
            "package exceeds {} MiB",
            package::MAX_PACKAGE_BYTES / (1024 * 1024)
        ));
    }

    let skeleton = read_skeleton(&dir)?;
    let (manifest_bytes, parsed) =
        assemble_manifest(&dir, &mut sources, skeleton.unwrap_or_default(), options)?;

    let arch = parsed
        .platform
        .split('-')
        .next()
        .filter(|arch| !arch.is_empty())
        .unwrap_or("unknown");
    let path = options.out.clone().unwrap_or_else(|| {
        PathBuf::from(format!("{}-{}-{}.spk", parsed.name, parsed.version, arch))
    });
    let absolute = absolute_output(&path)?;
    if absolute.starts_with(&dir) {
        return Err("package output must be outside the bundle directory".into());
    }

    let key_id = match &options.key_id {
        Some(key_id) => key_id.clone(),
        None => default_key_id(&key_path)?,
    };
    if !package::valid_key_id(&key_id) {
        return Err(format!("invalid keyId: {} (pass --key-id)", key_id));
    }
    let signing_key = read_private_key(&key_path)?;
    let signature = signing_key.sign(&manifest_bytes);
    let signature = serde_json::json!({
        "alg": "ed25519",
        "keyId": key_id,
        "sig": BASE64.encode(signature.to_bytes()),
    });
    let signature_bytes = serde_json::to_vec(&signature)
        .map_err(|error| format!("serialize signature: {}", error))?;

    let mut entries: Vec<(&str, &[u8], u32)> = vec![
        ("manifest.json", &manifest_bytes, 0o644),
        ("signature.json", &signature_bytes, 0o644),
    ];
    for source in &sources {
        entries.push((source.path.as_str(), source.data.as_slice(), source.mode));
    }
    let archive = package::write_tar(&entries)?;
    if archive.len() as u64 > package::MAX_PACKAGE_BYTES {
        return Err(format!(
            "package exceeds {} MiB",
            package::MAX_PACKAGE_BYTES / (1024 * 1024)
        ));
    }

    fs::write(&path, &archive).map_err(|error| format!("write {}: {}", path.display(), error))?;

    Ok(path)
}

fn assemble_manifest(
    dir: &Path,
    sources: &mut [SourceFile],
    mut manifest: Map<String, Value>,
    options: &Options,
) -> Result<(Vec<u8>, Manifest), String> {
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
                "cannot derive the plugin id from the directory name; pass --id".to_string()
            })?,
    };
    if !package::valid_name(&name) {
        return Err(format!(
            "invalid plugin id: {} (expected [a-z0-9][a-z0-9_-]{{0,63}}; pass --id)",
            name
        ));
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
        return Err(format!(
            "version is not semver: {} (pass --version)",
            version
        ));
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
        return Err("desc must not be empty (pass --desc)".into());
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
            None => detect_exec(sources)?,
        },
    };
    if !sources.iter().any(|source| source.path == exec_path) {
        return Err(format!("exec.path is not in files[]: {}", exec_path));
    }
    package::check_path(&exec_path)?;

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
                return Err(format!("hook must start with a shebang: {}", path));
            }
        }
    }
    for source in sources.iter_mut() {
        source.mode = effective_mode(&source.path, source.mode, &exec_path, &hook_paths)?;
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

    manifest.entry("schema").or_insert_with(|| Value::from(1));
    manifest.entry("protocol").or_insert_with(|| Value::from(1));
    manifest.insert("name".into(), Value::String(name));
    manifest.insert("version".into(), Value::String(version));
    manifest.insert("desc".into(), Value::String(desc));
    manifest.insert(
        "platform".into(),
        Value::String(package::target().to_string()),
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
        .map_err(|error| format!("serialize manifest: {}", error))?;
    let parsed = package::parse_manifest(&manifest_bytes)?;
    package::validate_manifest(&parsed)?;
    Ok((manifest_bytes, parsed))
}

fn absolute_output(path: &Path) -> Result<PathBuf, String> {
    let absolute = std::path::absolute(path)
        .map_err(|error| format!("resolve {}: {}", path.display(), error))?;
    let parent = match absolute.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let parent = fs::canonicalize(parent)
        .map_err(|error| format!("resolve {}: {}", path.display(), error))?;
    Ok(match absolute.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    })
}

fn collect(root: &Path) -> Result<Vec<SourceFile>, String> {
    let mut sources = Vec::new();
    walk(root, root, &mut sources)?;
    sources.sort_by(|left, right| left.path.cmp(&right.path));
    let mut seen = BTreeSet::new();
    for source in &sources {
        if !seen.insert(source.path.clone()) {
            return Err(format!("duplicate path: {}", source.path));
        }
    }
    Ok(sources)
}

fn walk(root: &Path, directory: &Path, out: &mut Vec<SourceFile>) -> Result<(), String> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(directory)
        .map_err(|error| format!("read {}: {}", directory.display(), error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {}: {}", directory.display(), error))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let relative = relative_path(root, &path)?;
        if relative == ".." || relative.starts_with("../") {
            return Err(format!("unsafe path: {}", relative));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("stat {}: {}", path.display(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("symbolic links are not allowed: {}", relative));
        }
        if metadata.is_dir() {
            walk(root, &path, out)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!("not a regular file: {}", relative));
        }
        if matches!(relative.as_str(), "manifest.json" | "signature.json") {
            continue;
        }
        #[cfg(windows)]
        {
            let file =
                fs::File::open(&path).map_err(|error| format!("read {}: {}", relative, error))?;
            let info = windows_file_info(&file, &relative)?;
            if info.NumberOfLinks > 1 {
                return Err(format!("hard links are not allowed: {}", relative));
            }
            if info.EndOfFile > info.AllocationSize {
                return Err(format!("sparse files are not allowed: {}", relative));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(format!("hard links are not allowed: {}", relative));
            }
            if metadata.len() > 0 && metadata.blocks().saturating_mul(512) < metadata.len() {
                return Err(format!("sparse files are not allowed: {}", relative));
            }
        }
        let data = fs::read(&path).map_err(|error| format!("read {}: {}", relative, error))?;
        out.push(SourceFile {
            path: relative,
            data,
            mode: raw_mode(&metadata, &path)?,
        });
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> Result<String, String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("{} is outside {}", path.display(), root.display()))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| format!("non-UTF-8 file name under {}", root.display()))?;
                if name.contains('\\') {
                    return Err(format!("backslash in file name: {}", path.display()));
                }
                parts.push(name.to_string());
            }
            _ => return Err(format!("unsafe path: {}", relative.display())),
        }
    }
    Ok(parts.join("/"))
}

#[cfg(windows)]
fn windows_file_info(
    file: &fs::File,
    relative: &str,
) -> Result<windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO, String> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileStandardInfo, GetFileInformationByHandleEx, FILE_STANDARD_INFO,
    };

    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as _,
            FileStandardInfo,
            &mut info as *mut _ as *mut _,
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(format!(
            "stat {}: {}",
            relative,
            std::io::Error::last_os_error()
        ));
    }
    Ok(info)
}

#[cfg(unix)]
fn raw_mode(metadata: &fs::Metadata, path: &Path) -> Result<u32, String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    if mode & 0o6000 != 0 {
        return Err(format!("setuid/setgid is not allowed: {}", path.display()));
    }
    Ok(mode & 0o777)
}

#[cfg(not(unix))]
fn raw_mode(_metadata: &fs::Metadata, _path: &Path) -> Result<u32, String> {
    Ok(0)
}

fn effective_mode(
    path: &str,
    raw: u32,
    exec: &str,
    hooks: &BTreeSet<String>,
) -> Result<u32, String> {
    #[cfg(not(unix))]
    let _ = raw;
    let wants_executable_mode = path == exec || hooks.contains(path);
    if wants_executable_mode {
        #[cfg(unix)]
        {
            if raw & 0o111 == 0 {
                return Err(format!("{} is not executable", path));
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
            Ok(0o644)
        }
    }
}

fn normalize_exec(value: &str) -> Result<String, String> {
    let value = value.strip_prefix("./").unwrap_or(value);
    package::check_path(value)?;
    Ok(value.to_string())
}

fn detect_exec(sources: &[SourceFile]) -> Result<String, String> {
    let mut candidates: Vec<String> = sources
        .iter()
        .filter(|source| is_exec_candidate_location(&source.path) && looks_executable(source))
        .map(|source| source.path.clone())
        .collect();
    candidates.sort();
    candidates.dedup();
    match candidates.len() {
        0 => Err("cannot determine exec.path; pass --exec <path>".into()),
        1 => Ok(candidates.remove(0)),
        _ => Err(format!(
            "multiple executable candidates: {}; pass --exec <path>",
            candidates.join(", ")
        )),
    }
}

fn is_exec_candidate_location(path: &str) -> bool {
    match path.strip_prefix("payload/bin/") {
        Some(rest) => !rest.contains('/'),
        None => !path.contains('/'),
    }
}

fn looks_executable(source: &SourceFile) -> bool {
    #[cfg(unix)]
    {
        source.mode & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        match source.path.rsplit_once('.').map(|(_, extension)| extension) {
            Some(extension) => matches!(
                extension.to_ascii_lowercase().as_str(),
                "exe" | "cmd" | "bat" | "com"
            ),
            None => false,
        }
    }
}

fn read_skeleton(dir: &Path) -> Result<Option<Map<String, Value>>, String> {
    let path = dir.join("manifest.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("stat {}: {}", path.display(), error)),
    };
    if metadata.file_type().is_symlink() {
        return Err("symbolic links are not allowed: manifest.json".into());
    }
    if !metadata.is_file() {
        return Err("manifest.json must be a regular file".into());
    }
    let bytes = fs::read(&path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {}", path.display(), error))?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err("manifest.json must contain a JSON object".into()),
    }
}

fn default_key_id(path: &Path) -> Result<String, String> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
        .ok_or_else(|| "cannot derive keyId from the key file name; pass --key-id".to_string())
}

fn read_private_key(path: &Path) -> Result<SigningKey, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("read key {}: {}", path.display(), error))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("key {} is not an Ed25519 PKCS#8 PEM", path.display()))?;
    let body = pem_body(&text, "PRIVATE KEY")
        .ok_or_else(|| format!("key {} is not an Ed25519 PKCS#8 PEM", path.display()))?;
    let body: String = body.lines().map(str::trim).collect();
    let der = BASE64
        .decode(body)
        .map_err(|error| format!("key {} base64: {}", path.display(), error))?;
    const PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    if der.len() != 48 || !der.starts_with(&PREFIX) {
        return Err(format!(
            "key {} is not an Ed25519 PKCS#8 PEM",
            path.display()
        ));
    }
    let seed: [u8; 32] = der[16..]
        .try_into()
        .map_err(|_| "Ed25519 private key length".to_string())?;
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
mod tests {
    use super::*;
    use crate::package::tempdir::TempDir;
    use ed25519_dalek::Signature;
    use std::fs;

    fn write(dir: &Path, path: &str, data: &[u8], mode: u32) {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, data).expect("write file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
        }
    }

    fn pem_from_private(seed: &[u8; 32]) -> Vec<u8> {
        const PREFIX: [u8; 16] = [
            0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22,
            0x04, 0x20,
        ];
        let mut der = PREFIX.to_vec();
        der.extend_from_slice(seed);
        let body = BASE64.encode(der);
        let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
        for chunk in body.as_bytes().chunks(48) {
            pem.push_str(std::str::from_utf8(chunk).expect("base64"));
            pem.push('\n');
        }
        pem.push_str("-----END PRIVATE KEY-----\n");
        pem.into_bytes()
    }

    fn key_file(work: &TempDir, seed: u8) -> PathBuf {
        let path = work.path().join("test.key");
        fs::write(&path, pem_from_private(&[seed; 32])).expect("key");
        path
    }

    fn bundle_options(dir: &Path, out: &Path, key: &Path) -> Options {
        Options {
            dir: dir.to_path_buf(),
            out: Some(out.to_path_buf()),
            key: Some(key.to_path_buf()),
            key_id: Some("test".into()),
            ..Options::default()
        }
    }

    fn entries(bytes: &[u8]) -> Vec<package::TarEntry> {
        package::read_tar(bytes).expect("tar")
    }

    fn assert_checksums(bytes: &[u8]) {
        let mut offset = 0;
        loop {
            let header = &bytes[offset..offset + 512];
            if header.iter().all(|byte| *byte == 0) {
                break;
            }
            let stored = std::str::from_utf8(&header[148..156]).expect("checksum");
            let stored = stored.replace('\0', " ");
            let stored = usize::from_str_radix(stored.trim(), 8).expect("octal checksum");
            let mut copy = header.to_vec();
            for byte in &mut copy[148..156] {
                *byte = b' ';
            }
            let sum: usize = copy.iter().map(|byte| *byte as usize).sum();
            assert_eq!(sum, stored, "tar checksum");
            let size = std::str::from_utf8(&header[124..136]).expect("size");
            let size = usize::from_str_radix(size.trim_matches(char::from(0)).trim(), 8)
                .expect("octal size");
            offset += 512 + size + (512 - size % 512) % 512;
        }
    }

    #[test]
    fn builds_a_canonical_signed_package_from_a_directory() {
        let source = TempDir::new();
        let work = TempDir::new();
        let exec_path = if cfg!(windows) {
            "payload/bin/agent.cmd"
        } else {
            "payload/bin/agent"
        };
        write(source.path(), exec_path, b"#!/bin/sh\n", 0o755);
        write(
            source.path(),
            "payload/web/index.html",
            b"<html></html>",
            0o644,
        );
        write(source.path(), "config/config", b"{\"a\":1}", 0o644);
        let key = SigningKey::from_bytes(&[21u8; 32]);
        let key_path = key_file(&work, 21);
        let out = work.path().join("out.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("demo".into());
        options.version = Some("1.2.3".into());
        options.desc = Some("demo plugin".into());
        options.exec = Some(exec_path.into());

        let path = bundle(&options).expect("bundle");
        assert_eq!(path, out);

        let bytes = fs::read(&out).expect("read spk");
        assert_checksums(&bytes);
        let entries = entries(&bytes);
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(names[0], "manifest.json");
        assert_eq!(names[1], "signature.json");
        let expected = ["config/config", exec_path, "payload/web/index.html"];
        assert_eq!(&names[2..], &expected);

        let manifest_bytes = &entries[0].data;
        assert_ne!(manifest_bytes.last(), Some(&b'\n'));
        assert!(!manifest_bytes.starts_with(&[0xef, 0xbb, 0xbf]));
        let text = std::str::from_utf8(manifest_bytes).expect("manifest utf8");
        assert!(!text.contains(": "));
        assert!(!text.contains(", "));
        let mut position = 0;
        for key in [
            "\"config\"",
            "\"desc\"",
            "\"exec\"",
            "\"files\"",
            "\"name\"",
            "\"platform\"",
            "\"protocol\"",
            "\"schema\"",
            "\"ui\"",
            "\"version\"",
        ] {
            let found = text[position..]
                .find(key)
                .map(|offset| offset + position)
                .expect("sorted key");
            assert!(found >= position);
            position = found;
        }
        let value: Value = serde_json::from_slice(manifest_bytes).expect("manifest json");
        assert_eq!(
            serde_json::to_vec(&value).expect("canonical"),
            *manifest_bytes
        );
        let manifest = package::parse_manifest(manifest_bytes).expect("manifest");
        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.desc, "demo plugin");
        assert_eq!(manifest.platform, package::target());
        assert_eq!(manifest.exec.path, exec_path);
        assert_eq!(manifest.config.as_deref(), Some("config/config"));
        assert_eq!(manifest.ui.as_ref().map(|ui| ui.kind.as_str()), Some("web"));
        assert_eq!(manifest.files.len(), 3);
        let agent = manifest
            .files
            .iter()
            .find(|file| file.path == exec_path)
            .expect("exec file");
        assert_eq!(agent.mode, 0o755);
        assert_eq!(agent.size, b"#!/bin/sh\n".len() as u64);
        assert_eq!(agent.sha256, package::hex_sha256(b"#!/bin/sh\n"));

        let signature: Value = serde_json::from_slice(&entries[1].data).expect("signature");
        assert_eq!(signature["alg"], "ed25519");
        assert_eq!(signature["keyId"], "test");
        let signature_bytes = BASE64
            .decode(signature["sig"].as_str().expect("sig"))
            .expect("base64");
        key.verifying_key()
            .verify_strict(
                manifest_bytes,
                &Signature::from_slice(&signature_bytes).expect("signature"),
            )
            .expect("signature verifies");
    }

    #[test]
    fn generates_a_minimal_manifest_and_detects_the_only_executable() {
        let root = TempDir::new();
        let source = root.path().join("minimal");
        fs::create_dir_all(&source).expect("source");
        let work = TempDir::new();
        let exec_name = if cfg!(windows) { "tool.cmd" } else { "tool" };
        let exec_path = format!("payload/bin/{}", exec_name);
        write(&source, &exec_path, b"tool", 0o755);
        write(&source, "README.md", b"readme", 0o644);
        write(&source, "hooks/post-install", b"#!/bin/sh\n", 0o755);
        let key_path = key_file(&work, 22);
        let out = work.path().join("out.spk");
        let mut options = bundle_options(&source, &out, &key_path);
        options.id = Some("renamed".into());

        bundle(&options).expect("bundle");
        let entries = entries(&fs::read(&out).expect("read"));
        let manifest = package::parse_manifest(&entries[0].data).expect("manifest");
        assert_eq!(manifest.name, "renamed");
        assert_eq!(manifest.version, "0.1.0");
        assert_eq!(manifest.desc, "minimal");
        assert_eq!(manifest.exec.path, exec_path);
        assert_eq!(manifest.files.len(), 3);
        assert!(manifest.config.is_none());
        assert!(manifest.ui.is_none());
        assert_eq!(
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
            Some("hooks/post-install")
        );
    }

    #[test]
    fn requires_a_key_and_does_not_write_a_package() {
        let source = TempDir::new();
        let work = TempDir::new();
        let exec_path = if cfg!(windows) {
            "payload/bin/tool.cmd"
        } else {
            "payload/bin/tool"
        };
        write(source.path(), exec_path, b"tool", 0o755);
        let out = work.path().join("out.spk");
        let mut options = bundle_options(source.path(), &out, &work.path().join("missing.pem"));
        options.id = Some("demo".into());
        options.key = None;

        let error = bundle(&options).expect_err("missing key");
        assert!(error.contains("--key"), "{error}");
        assert!(!out.exists());
    }

    #[test]
    fn reports_ambiguous_or_missing_exec_candidates() {
        let work = TempDir::new();
        let key_path = key_file(&work, 23);

        let source = TempDir::new();
        let (one, two) = if cfg!(windows) {
            ("payload/bin/one.cmd", "payload/bin/two.cmd")
        } else {
            ("payload/bin/one", "payload/bin/two")
        };
        write(source.path(), one, b"one", 0o755);
        write(source.path(), two, b"two", 0o755);
        let mut ambiguous = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
        ambiguous.id = Some("ambiguous".into());
        let error = bundle(&ambiguous).expect_err("ambiguous");
        assert!(error.contains("multiple executable candidates"), "{error}");

        let source = TempDir::new();
        write(source.path(), "README.md", b"readme", 0o644);
        let mut missing = bundle_options(source.path(), &work.path().join("b.spk"), &key_path);
        missing.id = Some("missing".into());
        let error = bundle(&missing).expect_err("missing exec");
        assert!(error.contains("cannot determine exec.path"), "{error}");
    }

    #[test]
    fn overrides_author_fields_and_recomputes_files_from_a_manifest() {
        let source = TempDir::new();
        let work = TempDir::new();
        write(source.path(), "payload/bin/agent", b"agent", 0o755);
        write(
            source.path(),
            "payload/web/index.html",
            b"<html></html>",
            0o644,
        );
        write(source.path(), "config/config", b"{}", 0o644);
        let skeleton = serde_json::json!({
            "schema": 1,
            "name": "old",
            "version": "9.9.9",
            "desc": "old",
            "platform": "x86_64-unknown-linux-musl",
            "protocol": 1,
            "exec": {"path": "payload/bin/agent", "args": ["--stdio"], "ready": "exec"},
            "capabilities": [{"name": "status", "desc": "status", "inputSchema": {"type": "object"}}],
            "ui": {"type": "web", "entry": "index.html"},
            "files": [{"path": "stale", "size": 1, "sha256": "00", "mode": 420}]
        });
        fs::write(
            source.path().join("manifest.json"),
            serde_json::to_vec(&skeleton).expect("skeleton"),
        )
        .expect("manifest");
        let key_path = key_file(&work, 24);
        let out = work.path().join("out.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("fresh".into());
        options.version = Some("2.0.0".into());
        options.desc = Some("fresh desc".into());

        bundle(&options).expect("bundle");
        let entries = entries(&fs::read(&out).expect("read"));
        let manifest = package::parse_manifest(&entries[0].data).expect("manifest");
        assert_eq!(manifest.name, "fresh");
        assert_eq!(manifest.version, "2.0.0");
        assert_eq!(manifest.desc, "fresh desc");
        assert_eq!(manifest.platform, package::target());
        assert_eq!(manifest.exec.args, vec!["--stdio".to_string()]);
        assert_eq!(manifest.exec.ready.as_deref(), Some("exec"));
        assert_eq!(manifest.capabilities.len(), 1);
        assert_eq!(manifest.files.len(), 3);
        assert!(manifest.files.iter().all(|file| file.path != "stale"));
        assert!(manifest
            .files
            .iter()
            .all(|file| file.path != "manifest.json"));
        let names: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            &names[2..],
            &[
                "config/config",
                "payload/bin/agent",
                "payload/web/index.html"
            ]
        );
    }

    #[test]
    fn rejects_an_exec_path_outside_files_and_unsafe_paths() {
        let source = TempDir::new();
        let work = TempDir::new();
        write(source.path(), "payload/bin/tool", b"tool", 0o755);
        let key_path = key_file(&work, 26);
        let out = work.path().join("out.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("demo".into());
        options.exec = Some("payload/bin/missing".into());
        let error = bundle(&options).expect_err("missing exec");
        assert!(error.contains("exec.path is not in files[]"), "{error}");

        options.exec = Some("../escape".into());
        let error = bundle(&options).expect_err("unsafe exec");
        assert!(error.contains("unsafe path"), "{error}");

        options.exec = Some("/absolute/tool".into());
        let error = bundle(&options).expect_err("absolute exec");
        assert!(error.contains("unsafe path"), "{error}");

        options.exec = Some("payload\\bin\\tool".into());
        let error = bundle(&options).expect_err("backslash exec");
        assert!(error.contains("unsafe path"), "{error}");
    }

    #[test]
    fn rejects_a_declared_hook_that_is_not_in_files() {
        let source = TempDir::new();
        let work = TempDir::new();
        write(source.path(), "payload/bin/tool", b"tool", 0o755);
        let skeleton = serde_json::json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": "x86_64-unknown-linux-musl", "protocol": 1,
            "exec": {"path": "payload/bin/tool"},
            "hooks": {"postInstall": "hooks/post-install"}
        });
        fs::write(
            source.path().join("manifest.json"),
            serde_json::to_vec(&skeleton).expect("skeleton"),
        )
        .expect("manifest");
        let key_path = key_file(&work, 27);
        let out = work.path().join("out.spk");
        let error = bundle(&bundle_options(source.path(), &out, &key_path)).expect_err("hook");
        assert!(error.contains("postInstall is not in files[]"), "{error}");
    }

    #[test]
    fn refuses_to_write_the_package_inside_the_bundle_directory() {
        let source = TempDir::new();
        let work = TempDir::new();
        let exec_path = if cfg!(windows) {
            "payload/bin/tool.cmd"
        } else {
            "payload/bin/tool"
        };
        write(source.path(), exec_path, b"tool", 0o755);
        let key_path = key_file(&work, 32);
        let out = source.path().join("inside.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("demo".into());
        let error = bundle(&options).expect_err("output inside");
        assert!(error.contains("outside the bundle directory"), "{error}");
        assert!(!out.exists());
    }

    #[test]
    fn refuses_a_signing_key_inside_the_bundle_directory() {
        let source = TempDir::new();
        let exec_path = if cfg!(windows) {
            "payload/bin/tool.cmd"
        } else {
            "payload/bin/tool"
        };
        write(source.path(), exec_path, b"tool", 0o755);
        let key_path = source.path().join("signing.key");
        fs::write(&key_path, pem_from_private(&[31u8; 32])).expect("key");
        let out = source.path().join("out.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("demo".into());
        let error = bundle(&options).expect_err("key inside bundle");
        assert!(error.contains("inside the bundle directory"), "{error}");
        assert!(!out.exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_non_executable_exec_file() {
        let source = TempDir::new();
        let work = TempDir::new();
        write(source.path(), "payload/bin/tool", b"tool", 0o644);
        let key_path = key_file(&work, 33);
        let out = work.path().join("a.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("demo".into());
        options.exec = Some("payload/bin/tool".into());
        let error = bundle(&options).expect_err("non-executable");
        assert!(error.contains("is not executable"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn rejects_windows_hard_links() {
        let source = TempDir::new();
        let work = TempDir::new();
        write(source.path(), "payload/bin/tool.cmd", b"tool", 0o755);
        fs::hard_link(
            source.path().join("payload/bin/tool.cmd"),
            source.path().join("payload/bin/copy.cmd"),
        )
        .expect("hard link");
        let key_path = key_file(&work, 34);
        let out = work.path().join("a.spk");
        let mut options = bundle_options(source.path(), &out, &key_path);
        options.id = Some("demo".into());
        let error = bundle(&options).expect_err("hard link");
        assert!(error.contains("hard links are not allowed"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_hard_links() {
        use std::os::unix::fs::symlink;

        let work = TempDir::new();
        let key_path = key_file(&work, 28);

        let source = TempDir::new();
        write(source.path(), "payload/bin/tool", b"tool", 0o755);
        fs::create_dir_all(source.path().join("payload/web")).expect("web");
        symlink("payload/bin/tool", source.path().join("payload/web/alias")).expect("symlink");
        let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
        options.id = Some("symlink".into());
        let error = bundle(&options).expect_err("symlink");
        assert!(error.contains("symbolic links are not allowed"), "{error}");

        let source = TempDir::new();
        write(source.path(), "payload/bin/tool", b"tool", 0o755);
        fs::hard_link(
            source.path().join("payload/bin/tool"),
            source.path().join("payload/bin/copy"),
        )
        .expect("hard link");
        let mut hard_link_options =
            bundle_options(source.path(), &work.path().join("b.spk"), &key_path);
        hard_link_options.id = Some("hardlink".into());
        let error = bundle(&hard_link_options).expect_err("hard link");
        assert!(error.contains("hard links are not allowed"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_setuid_files() {
        use std::os::unix::fs::PermissionsExt;

        let source = TempDir::new();
        let work = TempDir::new();
        write(source.path(), "payload/bin/tool", b"tool", 0o755);
        fs::set_permissions(
            source.path().join("payload/bin/tool"),
            fs::Permissions::from_mode(0o4755),
        )
        .expect("setuid");
        let key_path = key_file(&work, 29);
        let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
        options.id = Some("setuid".into());
        let error = bundle(&options).expect_err("setuid");
        assert!(error.contains("setuid"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_sparse_files() {
        let source = TempDir::new();
        let work = TempDir::new();
        let path = source.path().join("payload/bin/tool");
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        let file = fs::File::create(&path).expect("create");
        file.set_len(1 << 20).expect("sparse");
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let key_path = key_file(&work, 30);
        let mut options = bundle_options(source.path(), &work.path().join("a.spk"), &key_path);
        options.id = Some("sparse".into());
        let error = bundle(&options).expect_err("sparse");
        assert!(error.contains("sparse files are not allowed"), "{error}");
    }
}
