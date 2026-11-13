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
    /// Host interpreter for script payloads; absent for native executables.
    #[serde(default)]
    pub interpreter: Option<Interpreter>,
    /// Optional start/stop/status actions supervised like a core service.
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
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
pub struct Interpreter {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Numeric constraint such as `>=3.10,<4`; absent/empty means presence only.
    #[serde(default)]
    pub version: Option<String>,
}

/// Plugin lifecycle actions, mirroring a core definition.
#[derive(Debug, Clone, Deserialize)]
pub struct Lifecycle {
    pub start: Option<Hook>,
    pub stop: Option<Hook>,
    pub status: Option<Hook>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Hook {
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Timeout in seconds; omitted uses the action default.
    #[serde(default)]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capability {
    pub name: String,
    pub desc: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ui {
    #[serde(rename = "type")]
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
    pub post_install: Option<String>,
    #[serde(rename = "preRemove")]
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

    /// Total bytes this package writes to disk: declared files plus the two
    /// package metadata files.
    pub fn unpacked_size(&self) -> u64 {
        let files: u64 = self.files.iter().map(|file| file.data.len() as u64).sum();
        files
            .saturating_add(self.manifest_bytes.len() as u64)
            .saturating_add(self.signature_bytes.len() as u64)
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
    validate_manifest(&manifest)?;
    if manifest.platform != target() {
        return Err(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            target()
        ));
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
    check_hooks(&manifest)?;

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
    #[cfg(unix)]
    check_hook_shebangs(&manifest, &files)?;
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|file| (file.path.as_str(), file.data.as_slice()))
        .collect();
    validate_script_payload(&manifest, &borrowed)?;

    Ok(Package {
        manifest,
        manifest_bytes,
        signature_bytes,
        files,
    })
}

/// The install-time half of the "executable hook" rule: the Unix exec bit comes
/// from the manifest, the shebang has to be read from the extracted file.
#[cfg(unix)]
fn check_hook_shebangs(manifest: &Manifest, files: &[FileData]) -> Result<(), String> {
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
        let Some(file) = files.iter().find(|file| file.path == path) else {
            continue;
        };
        if !file.data.starts_with(b"#!") {
            return Err(format!("{} must start with a shebang: {}", name, path));
        }
    }
    Ok(())
}

/// Validate every cross-reference a manifest makes, including the hook and
/// capability rules the install path enforces separately. The bundler calls
/// this before writing so authoring mistakes fail early.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    check_schema(manifest)?;
    check_ready(manifest)?;
    check_capabilities_with_exec(manifest)?;
    if manifest.files.is_empty() {
        return Err("package has no payload files".into());
    }
    check_manifest_paths(manifest)?;
    check_hooks(manifest)?;
    check_lifecycle(manifest)?;
    check_interpreter(manifest)?;
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
        if capability.name == "start" {
            return Err("capability name \"start\" is reserved for the lifecycle tool".into());
        }
        if !names.insert(&capability.name) {
            return Err(format!("duplicate capability: {}", capability.name));
        }
    }
    Ok(())
}

/// True when the payload bytes are a native executable (ELF, Mach-O or PE).
/// Anything else is treated as a script and needs a declared interpreter.
pub fn is_native_binary(bytes: &[u8]) -> bool {
    const MAGICS: [&[u8]; 10] = [
        b"\x7fELF",
        b"\xfe\xed\xfa\xce",
        b"\xfe\xed\xfa\xcf",
        b"\xce\xfa\xed\xfe",
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\xbe\xba\xfe\xca",
        b"\xca\xfe\xba\xbf",
        b"\xbf\xba\xfe\xca",
        b"MZ",
    ];
    MAGICS.iter().any(|magic| bytes.starts_with(magic))
}

/// Schema-2 packages must name a host interpreter for every script payload and
/// must not carry a dangling interpreter declaration for a native-only payload.
pub fn validate_script_payload(manifest: &Manifest, files: &[(&str, &[u8])]) -> Result<(), String> {
    if manifest.schema < 2 {
        return Ok(());
    }
    let mut executables = vec![manifest.exec.path.as_str()];
    if let Some(lifecycle) = &manifest.lifecycle {
        for hook in [&lifecycle.start, &lifecycle.stop, &lifecycle.status]
            .into_iter()
            .flatten()
        {
            executables.push(hook.path.as_str());
        }
    }
    let mut scripts = Vec::new();
    for path in executables {
        let Some((_, data)) = files.iter().find(|(candidate, _)| *candidate == path) else {
            // A missing payload file is reported by the declaration checks.
            continue;
        };
        if !is_native_binary(data) {
            scripts.push(path.to_string());
        }
    }
    if !scripts.is_empty() && manifest.interpreter.is_none() {
        return Err(format!(
            "script payload {} requires an interpreter declaration",
            scripts.join(", ")
        ));
    }
    if scripts.is_empty() && manifest.interpreter.is_some() {
        return Err("interpreter is declared but the payload has no script to run".into());
    }
    Ok(())
}

fn check_schema(manifest: &Manifest) -> Result<(), String> {
    match manifest.schema {
        1 | 2 => {}
        other => return Err(format!("unsupported schema {}", other)),
    }
    if manifest.schema == 1 && (manifest.interpreter.is_some() || manifest.lifecycle.is_some()) {
        return Err(
            "lifecycle and interpreter require schema 2; old daemons would silently ignore them"
                .into(),
        );
    }
    Ok(())
}

fn check_ready(manifest: &Manifest) -> Result<(), String> {
    match manifest.exec.ready.as_deref() {
        None | Some("notify") | Some("exec") => Ok(()),
        Some(other) => Err(format!(
            "exec.ready {:?} is invalid; expected notify or exec",
            other
        )),
    }
}

/// A capabilities list requires a protocol channel, which an exec-ready process
/// does not have (stdin is closed).
fn check_capabilities_with_exec(manifest: &Manifest) -> Result<(), String> {
    if !manifest.capabilities.is_empty() && manifest.exec.ready.as_deref() == Some("exec") {
        return Err(
            "exec.ready=exec cannot declare capabilities: the plugin has no protocol channel"
                .into(),
        );
    }
    Ok(())
}

fn check_lifecycle(manifest: &Manifest) -> Result<(), String> {
    let Some(lifecycle) = &manifest.lifecycle else {
        return Ok(());
    };
    let mut declared = false;
    for (name, hook, max_seconds) in [
        ("start", lifecycle.start.as_ref(), 300u64),
        ("stop", lifecycle.stop.as_ref(), 300u64),
        ("status", lifecycle.status.as_ref(), 30u64),
    ] {
        let Some(hook) = hook else {
            continue;
        };
        declared = true;
        check_path(&hook.path)?;
        if !manifest.files.iter().any(|file| file.path == hook.path) {
            return Err(format!(
                "lifecycle.{} is not in files[]: {}",
                name, hook.path
            ));
        }
        if let Some(timeout) = hook.timeout {
            if timeout == 0 || timeout > max_seconds {
                return Err(format!(
                    "lifecycle.{}.timeout must be between 1 and {} seconds",
                    name, max_seconds
                ));
            }
        }
        for argument in &hook.args {
            check_argument("lifecycle", name, argument)?;
        }
        for (key, value) in &hook.env {
            check_argument("lifecycle", name, key)?;
            check_argument("lifecycle", name, value)?;
        }
    }
    if !declared {
        return Err("lifecycle declares no start, stop or status action".into());
    }
    Ok(())
}

fn check_interpreter(manifest: &Manifest) -> Result<(), String> {
    let Some(interpreter) = &manifest.interpreter else {
        return Ok(());
    };
    if !crate::interpreter::valid_name(&interpreter.name) {
        return Err(format!(
            "interpreter.name {:?} is not a bare name (no paths or spaces)",
            interpreter.name
        ));
    }
    if let Some(version) = &interpreter.version {
        crate::interpreter::validate_constraint(version)
            .map_err(|error| format!("interpreter.version: {}", error))?;
    }
    for argument in &interpreter.args {
        check_argument("interpreter", &interpreter.name, argument)?;
    }
    Ok(())
}

fn check_argument(section: &str, name: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{} {} contains a NUL byte", section, name));
    }
    Ok(())
}

/// `true` when the manifest actually names at least one hook; an empty
/// `"hooks": {}` object is not an install-time hook package.
pub fn declares_hooks(manifest: &Manifest) -> bool {
    manifest
        .hooks
        .as_ref()
        .map(|hooks| hooks.post_install.is_some() || hooks.pre_remove.is_some())
        .unwrap_or(false)
}

/// Hooks are package-signed executables under `hooks/`; the manifest path is
/// resolved inside the installed version directory at run time.
fn check_hooks(manifest: &Manifest) -> Result<(), String> {
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
        let file = manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| format!("{} is not in files[]: {}", name, path))?;
        check_hook_executable(name, path, file.mode)?;
    }
    Ok(())
}

fn check_hook_executable(name: &str, path: &str, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        if mode & 0o111 == 0 {
            return Err(format!("{} must be executable: {}", name, path));
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no exec bit; the hook runner rejects a file that is not
        // .exe/.cmd/.bat when it actually runs it.
        let _ = (name, path, mode);
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
    let pem = fs::read(&key_path).map_err(|error| {
        format!(
            "unknown keyId {}: {} (trust root path: {}; add it with `svc trust add <file.pub>` or upload it in the web console)",
            sig.key_id,
            error,
            key_path.display()
        )
    })?;
    let key = parse_public_key(&pem)?;
    let signature = BASE64
        .decode(sig.sig.trim())
        .map_err(|error| format!("signature base64: {}", error))?;
    let signature = Signature::from_slice(&signature).map_err(|error| error.to_string())?;
    key.verify_strict(manifest, &signature)
        .map_err(|_| "manifest signature is invalid".to_string())
}

/// Validate a PEM SPKI Ed25519 public key without installing it.
pub fn validate_public_key(pem: &[u8]) -> Result<(), String> {
    parse_public_key(pem).map(|_| ())
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
    check_schema(&m)?;
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
    use serde_json::{json, Value};
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
        let mismatched = format!("{}-mismatch", target());
        let bytes = package_for(&key, &mismatched);
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
        assert!(error.contains("missing.pub"), "{error}");
        assert!(error.contains("trust root"), "{error}");
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
    fn rejects_a_capability_named_start() {
        let (bytes, _files) = manifest(target());
        let mut value: Value = serde_json::from_slice(&bytes).expect("manifest");
        value["capabilities"] = json!([
            {"name": "start", "desc": "reserved", "inputSchema": {"type": "object"}}
        ]);
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let manifest = parse_manifest(&manifest).expect("parse");

        let error = validate_manifest(&manifest).expect_err("reserved capability");

        assert!(error.contains("reserved"), "{error}");
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

    fn hook_file() -> (String, Vec<u8>) {
        #[cfg(windows)]
        {
            (
                "hooks/post-install.cmd".to_string(),
                b"@echo off\r\nexit /b 0\r\n".to_vec(),
            )
        }
        #[cfg(unix)]
        {
            (
                "hooks/post-install".to_string(),
                b"#!/bin/sh\nexit 0\n".to_vec(),
            )
        }
    }

    fn hook_package(key: &SigningKey, hook_path: &str, hook_mode: u32) -> Vec<u8> {
        let hook = if cfg!(unix) {
            b"#!/bin/sh\nexit 0\n".to_vec()
        } else {
            b"@echo off\r\nexit /b 0\r\n".to_vec()
        };
        hook_package_with_body(key, hook_path, hook_mode, hook)
    }

    fn hook_package_with_body(
        key: &SigningKey,
        hook_path: &str,
        hook_mode: u32,
        hook: Vec<u8>,
    ) -> Vec<u8> {
        let exec = b"hello".to_vec();
        let files = [
            ("payload/bin/demo", exec, 0o755),
            (hook_path, hook, hook_mode),
        ];
        let entries: Vec<serde_json::Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({
                    "path": path,
                    "size": data.len(),
                    "sha256": hex_sha256(data),
                    "mode": mode
                })
            })
            .collect();
        let value = json!({
            "schema": 1, "name": "demo", "version": "1.0.0", "desc": "demo",
            "platform": target(), "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "hooks": {"postInstall": hook_path},
            "files": entries
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        package_from(key, "test", &manifest, &files)
    }

    #[test]
    fn verifies_a_package_that_declares_hooks() {
        let key = setup();
        let (path, _) = hook_file();

        let package = verify_bytes(&hook_package(&key, &path, 0o755)).expect("hooks package");

        assert_eq!(
            package
                .manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
            Some(path.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_the_executable_bit() {
        let key = setup();
        let (path, _) = hook_file();

        let error = verify_bytes(&hook_package(&key, &path, 0o644)).expect_err("mode");

        assert!(error.contains("must be executable"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_hook_without_a_shebang() {
        let key = setup();
        let (path, _) = hook_file();

        let error = verify_bytes(&hook_package_with_body(
            &key,
            &path,
            0o755,
            b"exit 0\n".to_vec(),
        ))
        .expect_err("shebang");

        assert!(error.contains("shebang"), "{error}");
    }

    #[test]
    fn rejects_a_hook_outside_the_hooks_directory() {
        let key = setup();

        let error = verify_bytes(&hook_package(&key, "payload/hook", 0o755)).expect_err("path");

        assert!(error.contains("must be under hooks/"), "{error}");
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

    fn script_manifest(schema: u32) -> (Vec<u8>, TestFiles) {
        let files: TestFiles = vec![("payload/bin/clock.sh", b"#!/bin/sh\n".to_vec(), 0o755)];
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": schema,
            "name": "clock",
            "version": "1.0.0",
            "desc": "clock service",
            "platform": target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/clock.sh", "ready": "exec"},
            "interpreter": {"name": "sh"},
            "lifecycle": {
                "start": {"path": "payload/bin/clock.sh", "args": ["start"]},
                "stop": {"path": "payload/bin/clock.sh", "args": ["stop"]},
                "status": {"path": "payload/bin/clock.sh", "args": ["status"], "timeout": 5}
            },
            "files": entries
        });
        (serde_json::to_vec(&value).expect("manifest"), files)
    }

    #[test]
    fn rejects_lifecycle_or_interpreter_under_schema_one() {
        let (bytes, _) = script_manifest(1);
        let error = parse_manifest(&bytes).expect_err("schema gate");
        assert!(error.contains("schema 2"), "{error}");
    }

    #[test]
    fn accepts_lifecycle_and_interpreter_under_schema_two() {
        let (bytes, _) = script_manifest(2);
        let manifest = parse_manifest(&bytes).expect("parse");
        validate_manifest(&manifest).expect("validate");
        assert_eq!(
            manifest
                .interpreter
                .as_ref()
                .map(|interpreter| interpreter.name.as_str()),
            Some("sh")
        );
        let lifecycle = manifest.lifecycle.as_ref().expect("lifecycle");
        assert_eq!(
            lifecycle.start.as_ref().expect("start").args,
            ["start".to_string()]
        );
        assert_eq!(lifecycle.status.as_ref().expect("status").timeout, Some(5));
    }

    #[test]
    fn rejects_a_lifecycle_path_outside_files_and_bad_timeouts() {
        let value = |path: &str, timeout: Option<u64>| {
            let files = vec![json!({
                "path": "payload/bin/clock.sh",
                "size": 10,
                "sha256": "00",
                "mode": 0o755
            })];
            json!({
                "schema": 2, "name": "clock", "version": "1.0.0", "desc": "clock",
                "platform": target(), "protocol": 1,
                "exec": {"path": "payload/bin/clock.sh", "ready": "exec"},
                "interpreter": {"name": "sh"},
                "lifecycle": {"stop": {
                    "path": path,
                    "timeout": timeout
                }},
                "files": files
            })
        };
        let bytes = serde_json::to_vec(&value("missing/stop.sh", None)).expect("json");
        let error = validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("path");
        assert!(error.contains("not in files[]"), "{error}");

        let bytes = serde_json::to_vec(&value("payload/bin/clock.sh", Some(0))).expect("json");
        let error = validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("zero");
        assert!(error.contains("between 1 and"), "{error}");

        let bytes = serde_json::to_vec(&value("payload/bin/clock.sh", Some(301))).expect("json");
        let error =
            validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("too long");
        assert!(error.contains("between 1 and"), "{error}");
    }

    #[test]
    fn rejects_capabilities_on_an_exec_ready_plugin() {
        let (bytes, _) = script_manifest(2);
        let mut value: Value = serde_json::from_slice(&bytes).expect("json");
        value["capabilities"] = json!([{
            "name": "status", "desc": "status", "inputSchema": {}
        }]);
        let bytes = serde_json::to_vec(&value).expect("json");
        let error = validate_manifest(&parse_manifest(&bytes).expect("parse"))
            .expect_err("capabilities need a channel");
        assert!(error.contains("no protocol channel"), "{error}");
    }

    #[test]
    fn rejects_a_bad_interpreter_name_or_constraint() {
        let (bytes, _) = script_manifest(2);
        let mut value: Value = serde_json::from_slice(&bytes).expect("json");
        value["interpreter"]["name"] = json!("py -3");
        let bytes = serde_json::to_vec(&value).expect("json");
        let error =
            validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("bad name");
        assert!(error.contains("bare name"), "{error}");

        let mut value: Value = serde_json::from_slice(&script_manifest(2).0).expect("json");
        value["interpreter"]["version"] = json!("3.10");
        let bytes = serde_json::to_vec(&value).expect("json");
        let error =
            validate_manifest(&parse_manifest(&bytes).expect("parse")).expect_err("bad constraint");
        assert!(error.contains("version"), "{error}");
        assert!(error.contains("must start with"), "{error}");
    }

    #[test]
    fn verify_bytes_enforces_the_schema_two_gate() {
        let key = setup();
        let (manifest, files) = script_manifest(1);
        let bytes = package_from(&key, "test", &manifest, &files);
        let error = verify_bytes(&bytes).expect_err("schema one");
        assert!(error.contains("schema 2"), "{error}");
    }

    #[test]
    fn schema_two_scripts_require_an_interpreter() {
        let key = setup();
        let (manifest, files) = script_manifest(2);
        let mut value: Value = serde_json::from_slice(&manifest).expect("json");
        value.as_object_mut().expect("object").remove("interpreter");
        let manifest = serde_json::to_vec(&value).expect("json");
        let error = verify_bytes(&package_from(&key, "test", &manifest, &files))
            .expect_err("missing interpreter");
        assert!(error.contains("requires an interpreter"), "{error}");
    }

    #[test]
    fn a_dangling_interpreter_on_a_native_payload_is_rejected() {
        let key = setup();
        let files: TestFiles = vec![("payload/bin/demo", b"\x7fELFdemo".to_vec(), 0o755)];
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": 2,
            "name": "native",
            "version": "1.0.0",
            "desc": "native",
            "platform": target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo", "ready": "notify"},
            "interpreter": {"name": "sh"},
            "files": entries
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        let error = verify_bytes(&package_from(&key, "test", &manifest, &files))
            .expect_err("dangling interpreter");
        assert!(error.contains("no script"), "{error}");
    }

    #[test]
    fn schema_one_scripts_keep_the_legacy_direct_exec_path() {
        let key = setup();
        let files: TestFiles = vec![("payload/bin/demo", b"#!/bin/sh\n".to_vec(), 0o755)];
        let entries: Vec<Value> = files
            .iter()
            .map(|(path, data, mode)| {
                json!({"path": path, "size": data.len(), "sha256": hex_sha256(data), "mode": mode})
            })
            .collect();
        let value = json!({
            "schema": 1,
            "name": "legacy",
            "version": "1.0.0",
            "desc": "legacy",
            "platform": target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": entries
        });
        let manifest = serde_json::to_vec(&value).expect("manifest");
        verify_bytes(&package_from(&key, "test", &manifest, &files)).expect("legacy package");
    }

    #[test]
    fn verifies_a_schema_two_script_package() {
        let key = setup();
        let (manifest, files) = script_manifest(2);
        let package =
            verify_bytes(&package_from(&key, "test", &manifest, &files)).expect("script package");
        assert_eq!(
            package
                .manifest
                .lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.start.as_ref())
                .map(|hook| hook.path.as_str()),
            Some("payload/bin/clock.sh")
        );
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
