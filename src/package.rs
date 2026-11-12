//! `.spk` manifest, ustar parsing, signature verification and file extraction.
//!
//! A package is verified completely before anything is written to the plugin
//! tree: the manifest signature first, then the declared `files[]` (size,
//! sha256, mode), then the tar entries. `install` can therefore keep the old
//! version intact until the new tree is complete.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::paths;

pub(crate) const MAX_PACKAGE_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const MAX_FILES: usize = 4096;

/// Exact target triple the package was built for.
pub fn target() -> &'static str {
    env!("SVCD_TARGET")
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub name: String,
    pub version: String,
    pub desc: String,
    pub platform: String,
    pub protocol: u32,
    pub exec: Exec,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    pub ui: Option<Ui>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    pub hooks: Option<Hooks>,
    pub config: Option<String>,
    #[serde(rename = "configCheck")]
    pub config_check: Option<ConfigCheck>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Exec {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub ready: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    pub name: String,
    #[allow(dead_code)]
    pub desc: String,
    #[serde(rename = "inputSchema")]
    #[allow(dead_code)]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ui {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub kind: String,
    pub entry: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hooks {
    #[serde(rename = "postInstall")]
    #[allow(dead_code)]
    pub post_install: Option<String>,
    #[serde(rename = "preRemove")]
    #[allow(dead_code)]
    pub pre_remove: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigCheck {
    pub path: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct SignatureFile {
    alg: String,
    #[serde(rename = "keyId")]
    key_id: String,
    sig: String,
}

/// One verified file, in package order.
#[derive(Debug, Clone)]
pub struct FileData {
    pub path: String,
    pub data: Vec<u8>,
    pub mode: u32,
}

#[derive(Debug, Clone)]
pub struct TarEntry {
    pub path: String,
    pub data: Vec<u8>,
    #[allow(dead_code)]
    pub mode: u32,
}

pub fn read_tar(bytes: &[u8]) -> Result<Vec<TarEntry>, String> {
    let mut tar = Tar::new(bytes);
    let mut entries = Vec::new();
    while let Some(header) = tar.next()? {
        if header.kind == b'5' {
            continue;
        }
        let data = header
            .data
            .ok_or_else(|| format!("{} must be a regular file", header.path))?;
        entries.push(TarEntry {
            path: header.path,
            data: data.to_vec(),
            mode: header.mode & 0o777,
        });
    }
    Ok(entries)
}

pub fn write_tar(entries: &[(&str, &[u8], u32)]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (path, data, mode) in entries {
        check_path(path)?;
        if path.len() > 100 {
            return Err(format!("tar path is too long: {}", path));
        }
        let mut header = [0u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        let mode = format!("{:07o}", mode);
        header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
        let size = format!("{:011o}", data.len());
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
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.extend(std::iter::repeat_n(0, padding(data.len())));
    }
    out.extend(std::iter::repeat_n(0, 1024));
    Ok(out)
}

/// Rebuild a signed `.spk` from an installed version directory. The original
/// `manifest.json` bytes and `signature.json` are kept verbatim.
pub fn installed_spk(version_dir: &Path) -> Result<Vec<u8>, String> {
    let manifest_bytes = fs::read(version_dir.join("manifest.json"))
        .map_err(|error| format!("read manifest: {}", error))?;
    let manifest = parse_manifest(&manifest_bytes)?;
    let signature = fs::read(version_dir.join("signature.json"))
        .map_err(|error| format!("read signature: {}", error))?;
    let mut files = manifest.files.clone();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let mut owned: Vec<(String, Vec<u8>, u32)> = Vec::new();
    for file in files {
        let data = fs::read(version_dir.join(&file.path))
            .map_err(|error| format!("read {}: {}", file.path, error))?;
        if data.len() as u64 != file.size || hex_sha256(&data) != file.sha256 {
            return Err(format!("installed file is corrupt: {}", file.path));
        }
        owned.push((file.path, data, file.mode & 0o777));
    }
    let mut entries: Vec<(&str, &[u8], u32)> = vec![
        ("manifest.json", &manifest_bytes, 0o644),
        ("signature.json", &signature, 0o644),
    ];
    for (path, data, mode) in &owned {
        entries.push((path.as_str(), data.as_slice(), *mode));
    }
    write_tar(&entries)
}

#[derive(Debug)]
pub struct Package {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub signature_bytes: Vec<u8>,
    pub files: Vec<FileData>,
}

impl Package {
    pub fn write_to(&self, root: &Path) -> Result<(), String> {
        fs::create_dir_all(root)
            .map_err(|error| format!("create {}: {}", root.display(), error))?;
        write_one(&root.join("manifest.json"), &self.manifest_bytes, 0o644)?;
        write_one(&root.join("signature.json"), &self.signature_bytes, 0o644)?;
        for file in &self.files {
            let path = root.join(&file.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("create {}: {}", parent.display(), error))?;
            }
            write_one(&path, &file.data, file.mode)?;
        }
        Ok(())
    }

    /// The manifest's `config` template, if declared.
    pub fn config_template(&self) -> Option<&FileData> {
        let path = self.manifest.config.as_deref().unwrap_or("config/config");
        self.files.iter().find(|file| file.path == path)
    }
}

pub fn verify_package(path: &Path) -> Result<Package, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        return Err(format!(
            "package exceeds {} MiB",
            MAX_PACKAGE_BYTES / (1024 * 1024)
        ));
    }
    verify_bytes(&bytes)
}

pub fn verify_bytes(bytes: &[u8]) -> Result<Package, String> {
    let mut tar = Tar::new(bytes);

    let manifest_header = tar
        .next()?
        .ok_or_else(|| "manifest.json is missing".to_string())?;
    if manifest_header.path != "manifest.json" {
        return Err("manifest.json must be the first entry".into());
    }
    let manifest_bytes = manifest_header
        .data
        .ok_or_else(|| "manifest.json must be a regular file".to_string())?
        .to_vec();

    let signature_header = tar
        .next()?
        .ok_or_else(|| "signature.json is missing".to_string())?;
    if signature_header.path != "signature.json" {
        return Err("signature.json must be the second entry".into());
    }
    let signature_bytes = signature_header
        .data
        .ok_or_else(|| "signature.json must be a regular file".to_string())?
        .to_vec();
    verify_signature(&manifest_bytes, &signature_bytes)?;

    let manifest = parse_manifest(&manifest_bytes)?;
    if manifest.platform != target() {
        return Err(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            target()
        ));
    }
    if manifest.hooks.is_some() {
        return Err("hooks are not supported yet".into());
    }

    let mut declared = BTreeMap::new();
    for entry in &manifest.files {
        check_path(&entry.path)?;
        if entry.path == "manifest.json" || entry.path == "signature.json" {
            return Err("manifest.json/signature.json cannot be in files[]".to_string());
        }
        if declared.insert(entry.path.clone(), entry.clone()).is_some() {
            return Err(format!("duplicate files[] entry: {}", entry.path));
        }
        if entry.mode & 0o6000 != 0 {
            return Err(format!("setuid/setgid is not allowed: {}", entry.path));
        }
        if declared.len() > MAX_FILES {
            return Err("too many files".into());
        }
    }

    check_manifest_paths(&manifest)?;

    let mut files = Vec::new();
    let mut seen = BTreeMap::new();
    for header in tar {
        let header = header?;
        if header.kind == b'5' {
            continue;
        }
        let entry = declared
            .get(&header.path)
            .ok_or_else(|| format!("file is not declared in files[]: {}", header.path))?;
        if seen.insert(header.path.clone(), ()).is_some() {
            return Err(format!("duplicate tar entry: {}", header.path));
        }
        let data = header
            .data
            .ok_or_else(|| format!("{} must be a regular file", header.path))?;
        if data.len() as u64 != entry.size {
            return Err(format!(
                "size mismatch for {}: manifest {}, archive {}",
                entry.path,
                entry.size,
                data.len()
            ));
        }
        if hex_sha256(data) != entry.sha256 {
            return Err(format!("sha256 mismatch for {}", entry.path));
        }
        if header.mode & 0o777 != entry.mode & 0o777 {
            return Err(format!("mode mismatch for {}", entry.path));
        }
        files.push(FileData {
            path: header.path,
            data: data.to_vec(),
            mode: header.mode & 0o777,
        });
    }

    if seen.len() != declared.len() {
        let missing: Vec<&str> = declared
            .keys()
            .filter(|path| !seen.contains_key(*path))
            .map(String::as_str)
            .collect();
        return Err(format!("missing files: {}", missing.join(", ")));
    }
    if files.is_empty() {
        return Err("package has no payload files".into());
    }

    Ok(Package {
        manifest,
        manifest_bytes,
        signature_bytes,
        files,
    })
}

/// Validate every cross-reference a manifest makes, including the hook and
/// capability rules the install path enforces separately. The bundler calls
/// this before writing so authoring mistakes fail early.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.files.is_empty() {
        return Err("package has no payload files".into());
    }
    check_manifest_paths(manifest)?;
    for (name, path) in [
        (
            "postInstall",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
        ),
        (
            "preRemove",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.pre_remove.as_deref()),
        ),
    ] {
        let Some(path) = path else { continue };
        check_path(path)?;
        if !path.starts_with("hooks/") {
            return Err(format!("{} must be under hooks/: {}", name, path));
        }
        if !manifest.files.iter().any(|file| file.path == path) {
            return Err(format!("{} is not in files[]: {}", name, path));
        }
    }
    if let Some(ui) = &manifest.ui {
        if ui.kind == "web" {
            let entry = ui.entry.as_deref().unwrap_or("index.html");
            let path = format!("payload/web/{}", entry);
            check_path(&path)?;
            if !manifest.files.iter().any(|file| file.path == path) {
                return Err(format!("ui entry is not in files[]: {}", path));
            }
        }
    }
    let mut names = BTreeSet::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty() {
            return Err("capability name must not be empty".into());
        }
        if capability.name.starts_with("svc.") {
            return Err(format!(
                "capability may not use the svc. prefix: {}",
                capability.name
            ));
        }
        if !names.insert(&capability.name) {
            return Err(format!("duplicate capability: {}", capability.name));
        }
    }
    Ok(())
}

fn check_manifest_paths(manifest: &Manifest) -> Result<(), String> {
    check_path(&manifest.exec.path)?;
    if !manifest
        .files
        .iter()
        .any(|file| file.path == manifest.exec.path)
    {
        return Err(format!(
            "exec.path is not in files[]: {}",
            manifest.exec.path
        ));
    }
    if let Some(config) = &manifest.config {
        check_path(config)?;
        if !manifest.files.iter().any(|file| &file.path == config) {
            return Err(format!("config is not in files[]: {}", config));
        }
    }
    if let Some(check) = &manifest.config_check {
        check_path(&check.path)?;
        if !manifest.files.iter().any(|file| file.path == check.path) {
            return Err(format!(
                "configCheck.path is not in files[]: {}",
                check.path
            ));
        }
        let config = manifest.config.as_deref().unwrap_or("config/config");
        if !manifest.files.iter().any(|file| file.path == config) {
            return Err("configCheck is declared but config is missing".into());
        }
    }
    if let Some(ui) = &manifest.ui {
        if let Some(entry) = &ui.entry {
            check_path(entry)?;
        }
    }
    Ok(())
}

fn verify_signature(manifest: &[u8], signature_bytes: &[u8]) -> Result<(), String> {
    let sig: SignatureFile =
        serde_json::from_slice(signature_bytes).map_err(|error| format!("signature: {}", error))?;
    if sig.alg != "ed25519" {
        return Err(format!("unsupported signature algorithm: {}", sig.alg));
    }
    if !valid_key_id(&sig.key_id) {
        return Err(format!("invalid keyId: {}", sig.key_id));
    }
    let key_path = paths::trust_dir().join(format!("{}.pub", sig.key_id));
    let pem =
        fs::read(&key_path).map_err(|error| format!("unknown keyId {}: {}", sig.key_id, error))?;
    let key = parse_public_key(&pem)?;
    let signature = BASE64
        .decode(sig.sig.trim())
        .map_err(|error| format!("signature base64: {}", error))?;
    let signature = Signature::from_slice(&signature).map_err(|error| error.to_string())?;
    key.verify_strict(manifest, &signature)
        .map_err(|_| "manifest signature is invalid".to_string())
}

fn parse_public_key(pem: &[u8]) -> Result<VerifyingKey, String> {
    let text = String::from_utf8_lossy(pem);
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("-----BEGIN PUBLIC KEY-----") {
            inside = true;
        } else if line.starts_with("-----END PUBLIC KEY-----") {
            inside = false;
        } else if inside {
            body.push_str(line.trim());
        }
    }
    let der = BASE64
        .decode(body)
        .map_err(|error| format!("public key base64: {}", error))?;
    const SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let raw: [u8; 32] = if der.len() == 44 && der.starts_with(&SPKI_PREFIX) {
        der[12..]
            .try_into()
            .map_err(|_| "public key length".to_string())?
    } else if der.len() == 32 {
        der.try_into()
            .map_err(|_| "public key length".to_string())?
    } else {
        return Err("public key is not an Ed25519 SPKI".into());
    };
    VerifyingKey::from_bytes(&raw).map_err(|error| error.to_string())
}

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, String> {
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if m.schema != 1 {
        return Err(format!("unsupported schema {}", m.schema));
    }
    if m.protocol != 1 {
        return Err(format!("unsupported protocol {}", m.protocol));
    }
    if !valid_name(&m.name) {
        return Err("invalid service id".into());
    }
    if !m.platform.contains('-') {
        return Err("platform must be a target triple".into());
    }
    check_version(&m.version)?;
    Ok(m)
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .next()
            .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn valid_key_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && s != "."
        && s != ".."
}

fn check_version(value: &str) -> Result<(), String> {
    if !valid_version(value) {
        return Err(format!("version is not semver: {}", value));
    }
    Ok(())
}

pub fn valid_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

pub fn check_path(p: &str) -> Result<(), String> {
    if p.starts_with('/') || p.contains('\\') || p.contains('\0') {
        return Err(format!("unsafe path: {}", p));
    }
    if p.split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err(format!("unsafe path: {}", p));
    }
    Ok(())
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{:02x}", byte);
    }
    out
}

fn write_one(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    let mut handle =
        File::create(path).map_err(|error| format!("create {}: {}", path.display(), error))?;
    handle
        .write_all(data)
        .map_err(|error| format!("write {}: {}", path.display(), error))?;
    handle
        .sync_all()
        .map_err(|error| format!("sync {}: {}", path.display(), error))?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(|error| format!("chmod {}: {}", path.display(), error))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

struct Tar<'a> {
    bytes: &'a [u8],
    offset: usize,
    done: bool,
}

struct Header<'a> {
    path: String,
    mode: u32,
    kind: u8,
    data: Option<&'a [u8]>,
}

impl<'a> Tar<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Tar {
            bytes,
            offset: 0,
            done: false,
        }
    }

    fn next(&mut self) -> Result<Option<Header<'a>>, String> {
        if self.done {
            return Ok(None);
        }
        let end = self.offset + 512;
        if end > self.bytes.len() {
            return Err("truncated tar header".into());
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
            return Err(format!("unsupported tar entry type for {}", path));
        }
        if kind == b'5' {
            path = path.trim_end_matches('/').to_string();
        }
        check_path(&path)?;
        let data = if kind == b'0' {
            let start = self.offset;
            let end = start
                .checked_add(size)
                .ok_or_else(|| "tar size overflow".to_string())?;
            if end > self.bytes.len() {
                return Err("truncated tar data".into());
            }
            self.offset = end + padding(size);
            Some(&self.bytes[start..end])
        } else {
            if size != 0 {
                return Err(format!("directory entry has data: {}", path));
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
    type Item = Result<Header<'a>, String>;

    fn next(&mut self) -> Option<Self::Item> {
        match Tar::next(self) {
            Ok(Some(header)) => Some(Ok(header)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

fn padding(size: usize) -> usize {
    let rem = size % 512;
    if rem == 0 {
        0
    } else {
        512 - rem
    }
}

fn field(hdr: &[u8], start: usize, len: usize) -> String {
    let slice = &hdr[start..start + len];
    let end = slice.iter().position(|b| *b == 0).unwrap_or(len);
    String::from_utf8_lossy(&slice[..end]).to_string()
}

fn octal(s: String) -> usize {
    usize::from_str_radix(s.trim_matches(char::from(0)).trim(), 8).unwrap_or(0)
}

/// Reproduce a package in memory for tests and the future bundler.
#[cfg(test)]
pub fn pem_from_public(key: &[u8; 32]) -> Vec<u8> {
    const PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let mut der = PREFIX.to_vec();
    der.extend_from_slice(key);
    let body = BASE64.encode(der);
    let mut pem = String::from("-----BEGIN PUBLIC KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64"));
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    pem.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::sync::OnceLock;

    static TRUST: OnceLock<tempdir::TempDir> = OnceLock::new();

    fn setup() -> SigningKey {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let dir = TRUST.get_or_init(|| {
            let dir = tempdir::TempDir::new();
            std::env::set_var("SVCD_TRUST_DIR", dir.path());
            std::fs::write(
                dir.path().join("test.pub"),
                pem_from_public(&key.verifying_key().to_bytes()),
            )
            .expect("write key");
            dir
        });
        let _ = dir;
        key
    }

    type TestFiles = Vec<(&'static str, Vec<u8>, u32)>;

    fn manifest(platform: &str) -> (Vec<u8>, TestFiles) {
        let files = vec![
            ("payload/bin/demo", b"hello".to_vec(), 0o755),
            ("config/config", b"{}".to_vec(), 0o644),
        ];
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": 1,
            "name": "demo",
            "version": "1.0.0",
            "desc": "demo",
            "platform": platform,
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": entries,
            "config": "config/config"
        });
        let bytes = serde_json::to_vec(&value).expect("manifest");
        (bytes, files)
    }

    fn package_for(key: &SigningKey, platform: &str) -> Vec<u8> {
        let (manifest, files) = manifest(platform);
        package_from(key, "test", &manifest, &files)
    }

    fn package_from(
        key: &SigningKey,
        key_id: &str,
        manifest: &[u8],
        files: &[(&str, Vec<u8>, u32)],
    ) -> Vec<u8> {
        let sig = key.sign(manifest);
        let signature = json!({"alg":"ed25519","keyId":key_id,"sig":BASE64.encode(sig.to_bytes())});
        let signature = serde_json::to_vec(&signature).expect("signature");
        let mut tar = Vec::new();
        tar.extend_from_slice(&header("manifest.json", manifest.len(), 0o644));
        tar.extend_from_slice(manifest);
        tar.extend(std::iter::repeat_n(0, padding(manifest.len())));
        tar.extend_from_slice(&header("signature.json", signature.len(), 0o644));
        tar.extend_from_slice(&signature);
        tar.extend(std::iter::repeat_n(0, padding(signature.len())));
        for (path, data, mode) in files {
            tar.extend_from_slice(&header(path, data.len(), *mode));
            tar.extend_from_slice(data);
            tar.extend(std::iter::repeat_n(0, padding(data.len())));
        }
        tar.extend(std::iter::repeat_n(0, 1024));
        tar
    }

    fn package(key: &SigningKey) -> Vec<u8> {
        package_for(key, target())
    }

    fn header(path: &str, size: usize, mode: u32) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        let mode = format!("{:07o}", mode);
        header[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
        let size = format!("{:011o}", size);
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header
    }

    #[test]
    fn verifies_a_signed_package_and_extracts_it() {
        let key = setup();
        let package = verify_bytes(&package(&key)).expect("verify");
        assert_eq!(package.manifest.name, "demo");
        assert_eq!(package.files.len(), 2);
        let root = tempdir::TempDir::new();
        package.write_to(root.path()).expect("write");
        assert_eq!(
            std::fs::read(root.path().join("payload/bin/demo")).expect("read"),
            b"hello"
        );
    }

    #[test]
    fn rejects_a_tampered_manifest() {
        let key = setup();
        let mut bytes = package(&key);
        let needle = b"\"demo\"";
        let position = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("name");
        bytes[position + 1] = b'D';
        assert!(verify_bytes(&bytes).is_err());
    }

    #[test]
    fn rejects_a_tampered_file() {
        let key = setup();
        let mut bytes = package(&key);
        let needle = b"hello";
        let position = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("payload");
        bytes[position] = b'j';
        assert!(verify_bytes(&bytes).is_err());
    }

    #[test]
    fn rejects_a_platform_mismatch() {
        let key = setup();
        let bytes = package_for(&key, "x86_64-unknown-linux-gnu");
        let error = verify_bytes(&bytes).expect_err("platform");
        assert!(error.contains("platform mismatch"), "{error}");
    }

    #[test]
    fn rejects_an_unknown_key_id() {
        let key = setup();
        let (manifest, files) = manifest(target());
        let bytes = package_from(&key, "missing", &manifest, &files);
        let error = verify_bytes(&bytes).expect_err("unknown key");
        assert!(error.contains("unknown keyId"), "{error}");
    }

    #[test]
    fn rejects_an_unsafe_path() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": target(), "protocol": 1,
            "exec": {"path": "../escape"},
            "files": [{"path": "../escape", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(&key, "test", &manifest, &[("../escape", data, 0o755)]);
        let error = verify_bytes(&bytes).expect_err("unsafe path");
        assert!(error.contains("unsafe path"), "{error}");
    }

    #[test]
    fn rejects_setuid_and_setgid() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": target(), "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": [{"path": "payload/bin/demo", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o4755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(
            &key,
            "test",
            &manifest,
            &[("payload/bin/demo", data, 0o4755)],
        );
        let error = verify_bytes(&bytes).expect_err("setuid");
        assert!(error.contains("setuid"), "{error}");
    }

    #[test]
    fn rejects_a_protocol_mismatch() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": target(), "protocol": 2,
            "exec": {"path": "payload/bin/demo"},
            "files": [{"path": "payload/bin/demo", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(
            &key,
            "test",
            &manifest,
            &[("payload/bin/demo", data, 0o755)],
        );
        let error = verify_bytes(&bytes).expect_err("protocol");
        assert!(error.contains("protocol"), "{error}");
    }

    #[test]
    fn rejects_hooks_until_the_hook_runner_lands() {
        let key = setup();
        let data = b"payload".to_vec();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": target(), "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "hooks": {"postInstall": "hooks/post-install.sh"},
            "files": [{"path": "payload/bin/demo", "size": data.len(), "sha256": hex_sha256(&data), "mode": 0o755}]
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let bytes = package_from(
            &key,
            "test",
            &manifest,
            &[("payload/bin/demo", data, 0o755)],
        );
        let error = verify_bytes(&bytes).expect_err("hooks");
        assert!(error.contains("hooks"), "{error}");
    }

    #[test]
    fn rejects_an_extra_tar_entry() {
        let key = setup();
        let mut bytes = package(&key);
        let end = bytes.len() - 1024;
        bytes.truncate(end);
        bytes.extend_from_slice(&header("extra.txt", 3, 0o644));
        bytes.extend_from_slice(b"bad");
        bytes.extend(std::iter::repeat_n(0, 509));
        bytes.extend(std::iter::repeat_n(0, 1024));
        let error = verify_bytes(&bytes).expect_err("extra");
        assert!(error.contains("not declared"), "{error}");
    }

    #[test]
    fn rejects_a_missing_declared_file() {
        let key = setup();
        let (manifest, mut files) = manifest(target());
        let missing = files.pop().expect("second file");
        let bytes = package_from(&key, "test", &manifest, &files);
        let error = verify_bytes(&bytes).expect_err("missing");
        assert!(error.contains("missing files"), "{error} {}", missing.0);
    }
}

#[cfg(test)]
pub mod tempdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "svcd-package-{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            TempDir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
