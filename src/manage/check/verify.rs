//! `.spk` 的校验与 manifest 解析：签名、trust、compat、路径与生命周期。
//!
//! 纯判定 + 读文件；不写目标树（staging 在 `manage/ops`）。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage::model::hex::hex_sha256;
use crate::manage::model::manifest::{
    FileData, FileEntry, Manifest, SignatureCheck, SignatureFile,
};
use crate::manage::model::manifest::{
    Inspection, Package, SignatureInfo, MAX_FILES, MAX_PACKAGE_BYTES,
};
use crate::manage::model::path::check_path;
use crate::manage::model::ustar::Tar;
use crate::wire::contract::RAW_PROTOCOL;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignatureMode {
    Required,
    AllowUnsigned,
    Inspect,
}

pub fn verify_package(path: &Path, trust_dir: &Path) -> Result<Package, ProblemError> {
    verify_package_with(path, SignatureCheck::Required, trust_dir)
}

pub fn verify_package_with(
    path: &Path,
    check: SignatureCheck,
    trust_dir: &Path,
) -> Result<Package, ProblemError> {
    let bytes = fs::read(path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    if bytes.len() as u64 > MAX_PACKAGE_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "package exceeds {} MiB",
            MAX_PACKAGE_BYTES / (1024 * 1024)
        )));
    }
    verify_bytes_with(&bytes, check, trust_dir)
}

pub fn verify_bytes(bytes: &[u8], trust_dir: &Path) -> Result<Package, ProblemError> {
    verify_bytes_with(bytes, SignatureCheck::Required, trust_dir)
}

pub fn verify_bytes_with(
    bytes: &[u8],
    check: SignatureCheck,
    trust_dir: &Path,
) -> Result<Package, ProblemError> {
    let mode = match check {
        SignatureCheck::Required => SignatureMode::Required,
        SignatureCheck::AllowUnsigned => SignatureMode::AllowUnsigned,
    };
    verify_bytes_mode(bytes, mode, None, trust_dir).map(|(package, _)| package)
}

/// Verify a package's own integrity without consulting host policy: no
/// platform match, no trust decision, no interpreter. The result carries the
/// signature facts so the caller can decide whether to trust them.
///
/// `public_key` may be an explicit PEM: a CI runner has no `trust.d`, so this
/// is how it checks the release signature without pretending the key is
/// trusted. `trusted` still reports whether the key happens to be in `trust.d`.
pub fn inspect_bytes_with(
    bytes: &[u8],
    public_key: Option<&[u8]>,
    trust_dir: &Path,
) -> Result<Inspection, ProblemError> {
    let explicit = match public_key {
        Some(pem) => Some(parse_public_key(pem)?),
        None => None,
    };
    let (package, signature) =
        verify_bytes_mode(bytes, SignatureMode::Inspect, explicit.as_ref(), trust_dir)?;
    Ok(Inspection { package, signature })
}

fn verify_bytes_mode(
    bytes: &[u8],
    mode: SignatureMode,
    explicit: Option<&VerifyingKey>,
    trust_dir: &Path,
) -> Result<(Package, SignatureInfo), ProblemError> {
    let mut tar = Tar::new(bytes);

    let manifest_header = tar
        .next()?
        .ok_or_else(|| Problem::Invalid.error("manifest.json is missing"))?;
    if manifest_header.path != "manifest.json" {
        return Err(Problem::Invalid.error("manifest.json must be the first entry"));
    }
    let manifest_bytes = manifest_header
        .data
        .ok_or_else(|| Problem::Invalid.error("manifest.json must be a regular file"))?
        .to_vec();

    let mut signature_bytes = Vec::new();
    let mut signature = SignatureInfo {
        state: "missing",
        key_id: None,
        trusted: false,
    };
    let mut first_file = None;
    match tar.next()? {
        Some(header) if header.path == "signature.json" => {
            let data = header
                .data
                .ok_or_else(|| Problem::Invalid.error("signature.json must be a regular file"))?;
            signature = check_signature(&manifest_bytes, data, mode, explicit, trust_dir)?;
            signature_bytes = data.to_vec();
        }
        Some(_) if mode == SignatureMode::Required => {
            return Err(Problem::Invalid.error("signature.json must be the second entry"));
        }
        // Insecure installs and inspection accept a package whose second entry
        // is already a payload file; it stays in the archive and is validated
        // like any other `files[]` entry below.
        Some(header) => first_file = Some(header),
        None if mode == SignatureMode::Required => {
            return Err(Problem::Invalid.error("signature.json is missing"));
        }
        None => {}
    }

    let manifest = parse_manifest(&manifest_bytes)?;
    validate_manifest(&manifest)?;
    // Inspection deliberately skips the host-platform gate: a package built
    // for another target is still a well-formed package, and the caller gets
    // `host.matchesHost` as a fact.
    if mode != SignatureMode::Inspect {
        check_platform(&manifest)?;
    }
    let declared = validate_declared_files(&manifest)?;

    let mut files = Vec::new();
    let mut seen = BTreeMap::new();
    let mut headers = Vec::new();
    if let Some(header) = first_file {
        headers.push(header);
    }
    for header in tar {
        headers.push(header?);
    }
    for header in headers {
        if header.kind == b'5' {
            continue;
        }
        let entry = declared.get(&header.path).ok_or_else(|| {
            Problem::Invalid.error(format!("file is not declared in files[]: {}", header.path))
        })?;
        if seen.insert(header.path.clone(), ()).is_some() {
            return Err(Problem::Invalid.error(format!("duplicate tar entry: {}", header.path)));
        }
        let data = header.data.ok_or_else(|| {
            Problem::Invalid.error(format!("{} must be a regular file", header.path))
        })?;
        if data.len() as u64 != entry.size {
            return Err(Problem::Invalid.error(format!(
                "size mismatch for {}: manifest {}, archive {}",
                entry.path,
                entry.size,
                data.len()
            )));
        }
        if hex_sha256(data) != entry.sha256 {
            return Err(Problem::Invalid.error(format!("sha256 mismatch for {}", entry.path)));
        }
        if header.mode & 0o777 != entry.mode & 0o777 {
            return Err(Problem::Invalid.error(format!("mode mismatch for {}", entry.path)));
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
        return Err(Problem::Invalid.error(format!("missing files: {}", missing.join(", "))));
    }
    if files.is_empty() {
        return Err(Problem::Invalid.error("package has no payload files"));
    }
    #[cfg(unix)]
    check_hook_shebangs(&manifest, &files)?;
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|file| (file.path.as_str(), file.data.as_slice()))
        .collect();
    validate_script_payload(&manifest, &borrowed)?;

    Ok((
        Package {
            manifest,
            manifest_bytes,
            signature_bytes,
            files,
        },
        signature,
    ))
}

/// Verify a package under the host-level insecure policy: strict verification
/// first, and only when it fails (and `allow_insecure` is true) retry with the
/// signature check skipped. The bool is true when the signature was verified;
/// false means the insecure fallback accepted the package.
pub fn verify_package_with_fallback(
    path: &Path,
    allow_insecure: bool,
    trust_dir: &Path,
) -> Result<(Package, bool), ProblemError> {
    if !allow_insecure {
        return verify_package(path, trust_dir).map(|package| (package, true));
    }
    match verify_package(path, trust_dir) {
        Ok(package) => Ok((package, true)),
        Err(_) => verify_package_with(path, SignatureCheck::AllowUnsigned, trust_dir)
            .map(|package| (package, false)),
    }
}

/// Byte-slice counterpart of [`verify_package_with_fallback`].
pub fn verify_bytes_with_fallback(
    bytes: &[u8],
    allow_insecure: bool,
    trust_dir: &Path,
) -> Result<(Package, bool), ProblemError> {
    if !allow_insecure {
        return verify_bytes(bytes, trust_dir).map(|package| (package, true));
    }
    match verify_bytes(bytes, trust_dir) {
        Ok(package) => Ok((package, true)),
        Err(_) => verify_bytes_with(bytes, SignatureCheck::AllowUnsigned, trust_dir)
            .map(|package| (package, false)),
    }
}

/// The install-time half of the "executable hook" rule: the Unix exec bit comes
/// from the manifest, the shebang has to be read from the extracted file.
#[cfg(unix)]
pub(crate) fn check_hook_shebangs(
    manifest: &Manifest,
    files: &[FileData],
) -> Result<(), ProblemError> {
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
            return Err(
                Problem::Invalid.error(format!("{} must start with a shebang: {}", name, path))
            );
        }
        if file.data.contains(&b'\r') {
            return Err(Problem::Invalid.error(format!(
                "{} must use LF line endings (the CRLF shebang fails to spawn): {}",
                name, path
            )));
        }
    }
    Ok(())
}

/// The host-platform gate. `Inspect` deliberately skips it: a package built
/// for another target is still well-formed, and the caller gets
/// `host.matchesHost` as a fact.
pub(crate) fn check_platform(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.platform != crate::platform::target() {
        return Err(Problem::UnsupportedPlatform.error(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            crate::platform::target()
        )));
    }
    Ok(())
}

/// The one `files[]` rule: reserved names, duplicate paths, setuid/setgid and
/// the file-count ceiling. Every install path calls this after
/// [`validate_manifest`], so `load_staged` cannot drift from `verify`.
pub(crate) fn validate_declared_files(
    manifest: &Manifest,
) -> Result<BTreeMap<String, FileEntry>, ProblemError> {
    let mut declared = BTreeMap::new();
    for entry in &manifest.files {
        check_path(&entry.path)?;
        if entry.path == "manifest.json" || entry.path == "signature.json" {
            return Err(Problem::Invalid.error("manifest.json/signature.json cannot be in files[]"));
        }
        if declared.insert(entry.path.clone(), entry.clone()).is_some() {
            return Err(Problem::Invalid.error(format!("duplicate files[] entry: {}", entry.path)));
        }
        if entry.mode & 0o6000 != 0 {
            return Err(
                Problem::Invalid.error(format!("setuid/setgid is not allowed: {}", entry.path))
            );
        }
        if declared.len() > MAX_FILES {
            return Err(Problem::TooLarge.error("too many files"));
        }
    }
    Ok(declared)
}

/// Validate every cross-reference a manifest makes, including the hook and
/// capability rules the install path enforces separately. The bundler calls
/// this before writing so authoring mistakes fail early.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), ProblemError> {
    check_schema(manifest)?;
    check_ready(manifest)?;
    check_capabilities_with_exec(manifest)?;
    check_compat(manifest)?;
    if manifest.files.is_empty() {
        return Err(Problem::Invalid.error("package has no payload files"));
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
                return Err(Problem::Invalid.error(format!("ui entry is not in files[]: {}", path)));
            }
        }
    }
    let mut names = BTreeSet::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty() {
            return Err(Problem::Invalid.error("capability name must not be empty"));
        }
        if capability.name.starts_with("svc.") {
            return Err(Problem::Invalid.error(format!(
                "capability may not use the svc. prefix: {}",
                capability.name
            )));
        }
        if capability.name == "start" {
            return Err(Problem::Invalid
                .error("capability name \"start\" is reserved for the lifecycle tool"));
        }
        if !names.insert(&capability.name) {
            return Err(
                Problem::Invalid.error(format!("duplicate capability: {}", capability.name))
            );
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
pub fn validate_script_payload(
    manifest: &Manifest,
    files: &[(&str, &[u8])],
) -> Result<(), ProblemError> {
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
        return Err(Problem::Invalid.error(format!(
            "script payload {} requires an interpreter declaration",
            scripts.join(", ")
        )));
    }
    if scripts.is_empty() && manifest.interpreter.is_some() {
        return Err(
            Problem::Invalid.error("interpreter is declared but the payload has no script to run")
        );
    }
    Ok(())
}

fn check_schema(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.schema == 0 || manifest.schema > MAX_SCHEMA {
        return Err(Problem::Invalid.error(format!(
            "unsupported schema {} (max {})",
            manifest.schema, MAX_SCHEMA
        )));
    }
    if manifest.schema == 1 && (manifest.interpreter.is_some() || manifest.lifecycle.is_some()) {
        return Err(Problem::Invalid.error(
            "lifecycle and interpreter require schema 2; old daemons would silently ignore them",
        ));
    }
    if manifest.schema < 3 && manifest.compat.is_some() {
        return Err(Problem::Invalid.error(
            "compat requires schema 3; old daemons would ignore the device list and install a \
             package that does not run here",
        ));
    }
    Ok(())
}

fn check_compat(manifest: &Manifest) -> Result<(), ProblemError> {
    let kernel_module = manifest
        .files
        .iter()
        .any(|file| file.path.to_ascii_lowercase().ends_with(".ko"));
    match &manifest.compat {
        Some(spec) => {
            if spec.devices.is_empty() {
                return Err(Problem::Invalid.error(
                    "compat.devices must not be empty; omit compat for a universal package",
                ));
            }
            if spec.devices.len() > 64 {
                return Err(Problem::Invalid.error("compat.devices lists more than 64 ids"));
            }
            let mut seen = BTreeSet::new();
            for device in &spec.devices {
                crate::platform::compat::validate_device_id(device).map_err(|error| {
                    Problem::Invalid.error(format!("compat.devices: {}", error))
                })?;
                if !seen.insert(device) {
                    return Err(
                        Problem::Invalid.error(format!("duplicate compat device id: {}", device))
                    );
                }
            }
        }
        None => {
            if kernel_module {
                return Err(Problem::Invalid.error(
                    "the package contains a kernel module (.ko) but declares no compat.devices; \
                     kernel modules only run on the device generation they were built for",
                ));
            }
        }
    }
    if kernel_module && manifest.schema < 3 {
        return Err(
            Problem::Invalid.error("kernel modules require schema 3 so old daemons reject them")
        );
    }
    Ok(())
}

fn check_ready(manifest: &Manifest) -> Result<(), ProblemError> {
    match manifest.exec.ready.as_deref() {
        None | Some("notify") | Some("exec") => Ok(()),
        Some(other) => Err(Problem::Invalid.error(format!(
            "exec.ready {:?} is invalid; expected notify or exec",
            other
        ))),
    }
}

/// A capabilities list requires a protocol channel, which an exec-ready process
/// does not have (stdin is closed).
fn check_capabilities_with_exec(manifest: &Manifest) -> Result<(), ProblemError> {
    if !manifest.capabilities.is_empty() && manifest.exec.ready.as_deref() == Some("exec") {
        return Err(Problem::Invalid.error(
            "exec.ready=exec cannot declare capabilities: the plugin has no protocol channel",
        ));
    }
    Ok(())
}

fn check_lifecycle(manifest: &Manifest) -> Result<(), ProblemError> {
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
            return Err(Problem::Invalid.error(format!(
                "lifecycle.{} is not in files[]: {}",
                name, hook.path
            )));
        }
        if let Some(timeout) = hook.timeout {
            if timeout == 0 || timeout > max_seconds {
                return Err(Problem::Invalid.error(format!(
                    "lifecycle.{}.timeout must be between 1 and {} seconds",
                    name, max_seconds
                )));
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
        return Err(Problem::Invalid.error("lifecycle declares no start, stop or status action"));
    }
    Ok(())
}

fn check_interpreter(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(interpreter) = &manifest.interpreter else {
        return Ok(());
    };
    if !crate::manage::check::interpreter::valid_name(&interpreter.name) {
        return Err(Problem::Invalid.error(format!(
            "interpreter.name {:?} is not a bare name (no paths or spaces)",
            interpreter.name
        )));
    }
    if let Some(version) = &interpreter.version {
        crate::manage::check::interpreter::validate_constraint(version)
            .map_err(|error| Problem::Invalid.error(format!("interpreter.version: {}", error)))?;
    }
    for argument in &interpreter.args {
        check_argument("interpreter", &interpreter.name, argument)?;
    }
    Ok(())
}

fn check_argument(section: &str, name: &str, value: &str) -> Result<(), ProblemError> {
    if value.contains('\0') {
        return Err(Problem::Invalid.error(format!("{} {} contains a NUL byte", section, name)));
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
pub(crate) fn check_hooks(manifest: &Manifest) -> Result<(), ProblemError> {
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
            return Err(Problem::Invalid.error(format!("{} must be under hooks/: {}", name, path)));
        }
        let file = manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                Problem::Invalid.error(format!("{} is not in files[]: {}", name, path))
            })?;
        check_hook_executable(name, path, file.mode)?;
    }
    Ok(())
}

fn check_hook_executable(name: &str, path: &str, mode: u32) -> Result<(), ProblemError> {
    #[cfg(unix)]
    {
        if mode & 0o111 == 0 {
            return Err(Problem::Invalid.error(format!("{} must be executable: {}", name, path)));
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

pub(crate) fn check_manifest_paths(manifest: &Manifest) -> Result<(), ProblemError> {
    check_path(&manifest.exec.path)?;
    if !manifest
        .files
        .iter()
        .any(|file| file.path == manifest.exec.path)
    {
        return Err(Problem::Invalid.error(format!(
            "exec.path is not in files[]: {}",
            manifest.exec.path
        )));
    }
    if let Some(config) = &manifest.config {
        check_path(config)?;
        if !manifest.files.iter().any(|file| &file.path == config) {
            return Err(Problem::Invalid.error(format!("config is not in files[]: {}", config)));
        }
    }
    if let Some(check) = &manifest.config_check {
        check_path(&check.path)?;
        if !manifest.files.iter().any(|file| file.path == check.path) {
            return Err(Problem::Invalid.error(format!(
                "configCheck.path is not in files[]: {}",
                check.path
            )));
        }
        let config = manifest.config.as_deref().unwrap_or("config/config");
        if !manifest.files.iter().any(|file| file.path == config) {
            return Err(Problem::Invalid.error("configCheck is declared but config is missing"));
        }
    }
    if let Some(ui) = &manifest.ui {
        if let Some(entry) = &ui.entry {
            check_path(entry)?;
        }
    }
    Ok(())
}

pub(crate) fn check_signature(
    manifest: &[u8],
    signature_bytes: &[u8],
    mode: SignatureMode,
    explicit: Option<&VerifyingKey>,
    trust_dir: &Path,
) -> Result<SignatureInfo, ProblemError> {
    if mode == SignatureMode::AllowUnsigned {
        // Insecure installs skip signature verification entirely. The bytes
        // are still kept in the installed tree; they are simply not trusted.
        return Ok(SignatureInfo {
            state: "unverified",
            key_id: None,
            trusted: false,
        });
    }
    let sig: SignatureFile = serde_json::from_slice(signature_bytes)
        .map_err(|error| Problem::Invalid.error(format!("signature: {}", error)))?;
    if sig.alg != "ed25519" {
        return Err(Problem::Invalid.error(format!("unsupported signature algorithm: {}", sig.alg)));
    }
    if !valid_key_id(&sig.key_id) {
        return Err(Problem::Invalid.error(format!("invalid keyId: {}", sig.key_id)));
    }
    let key_path = trust_dir.join(format!("{}.pub", sig.key_id));
    if mode == SignatureMode::Required {
        let pem = fs::read(&key_path)
            .map_err(|error| unknown_key_error(&sig.key_id, &key_path, error))?;
        let key = parse_public_key(&pem)?;
        verify_signature_with(manifest, &sig, &key)?;
        return Ok(SignatureInfo {
            state: "valid",
            key_id: Some(sig.key_id),
            trusted: true,
        });
    }
    // Inspection: verify with whichever key is available, and never claim
    // `valid` when nothing was actually checked. On a host without the key in
    // `trust.d` (a CI runner) that is `unknown-key`; `--pubkey` lets the caller
    // supply the key explicitly instead of faking a trust root.
    let trusted_key = fs::read(&key_path)
        .ok()
        .and_then(|pem| parse_public_key(&pem).ok());
    if let Some(key) = explicit.or(trusted_key.as_ref()) {
        verify_signature_with(manifest, &sig, key)?;
        return Ok(SignatureInfo {
            state: "valid",
            key_id: Some(sig.key_id),
            trusted: trusted_key.is_some(),
        });
    }
    Ok(SignatureInfo {
        state: "unknown-key",
        key_id: Some(sig.key_id),
        trusted: false,
    })
}

fn verify_signature_with(
    manifest: &[u8],
    sig: &SignatureFile,
    key: &VerifyingKey,
) -> Result<(), ProblemError> {
    let signature = BASE64
        .decode(sig.sig.trim())
        .map_err(|error| Problem::Invalid.error(format!("signature base64: {}", error)))?;
    let signature = Signature::from_slice(&signature)
        .map_err(|error| Problem::Internal.error(error.to_string()))?;
    key.verify_strict(manifest, &signature)
        .map_err(|_| Problem::Invalid.error("manifest signature is invalid"))
}

fn unknown_key_error(key_id: &str, key_path: &Path, error: std::io::Error) -> ProblemError {
    Problem::Invalid.error(format!(
        "unknown keyId {}: {} (trust root path: {}; add it with `svc trust add <file.pub>` or upload it in the web console)",
        key_id,
        error,
        key_path.display()
    ))
}

/// Validate a PEM SPKI Ed25519 public key without installing it.
pub fn validate_public_key(pem: &[u8]) -> Result<(), ProblemError> {
    parse_public_key(pem).map(|_| ())
}

fn parse_public_key(pem: &[u8]) -> Result<VerifyingKey, ProblemError> {
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
        .map_err(|error| Problem::Invalid.error(format!("public key base64: {}", error)))?;
    const SPKI_PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let raw: [u8; 32] = if der.len() == 44 && der.starts_with(&SPKI_PREFIX) {
        der[12..]
            .try_into()
            .map_err(|_| Problem::Invalid.error("public key length"))?
    } else if der.len() == 32 {
        der.try_into()
            .map_err(|_| Problem::Invalid.error("public key length"))?
    } else {
        return Err(Problem::Invalid.error("public key is not an Ed25519 SPKI"));
    };
    VerifyingKey::from_bytes(&raw).map_err(|error| Problem::Internal.error(error.to_string()))
}

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ProblemError> {
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|error| Problem::Invalid.error(error.to_string()))?;
    check_schema(&m)?;
    if m.protocol == 0 || m.protocol > MAX_PROTOCOL {
        return Err(Problem::Invalid.error(format!("unsupported protocol {}", m.protocol)));
    }
    if m.protocol == RAW_PROTOCOL {
        if m.exec.ready.as_deref() != Some("exec") {
            return Err(Problem::Invalid.error("protocol 3 requires exec.ready=exec"));
        }
        if !m.capabilities.is_empty() {
            return Err(Problem::Invalid.error("protocol 3 cannot declare JSON-RPC capabilities"));
        }
    }
    if !valid_name(&m.name) {
        return Err(Problem::Invalid.error("invalid service id"));
    }
    if !m.platform.contains('-') {
        return Err(Problem::UnsupportedPlatform.error("platform must be a target triple"));
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

fn check_version(value: &str) -> Result<(), ProblemError> {
    if !valid_version(value) {
        return Err(Problem::Invalid.error(format!("version is not semver: {}", value)));
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
