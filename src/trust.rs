//! Trust roots for `.spk` signatures: `<config>/trust.d/<keyId>.pub`.
//!
//! A package can never carry its own trust root, so provisioning is an explicit
//! admin action: `svc trust add <file.pub>`, an upload in the web console, or
//! dropping a PEM into `trust.d`. Only Ed25519 SPKI public keys are accepted.

use std::fs;
use std::path::PathBuf;

use crate::package;
use crate::paths;

/// Public keys are tiny; a larger file is a mistake, not a key.
pub const MAX_KEY_BYTES: u64 = 16 * 1024;

/// Sorted key ids present in `trust.d`; a missing directory means "none".
pub fn keys() -> Result<Vec<String>, String> {
    let directory = paths::trust_dir();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {}", directory.display(), error)),
    };
    let mut keys = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("pub") {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            if package::valid_key_id(stem) {
                keys.push(stem.to_string());
            }
        }
    }
    keys.sort();
    Ok(keys)
}

pub fn key_path(key_id: &str) -> PathBuf {
    paths::trust_dir().join(format!("{}.pub", key_id))
}

/// Validate and write `<keyId>.pub`; an existing file with the same id is
/// replaced so rotating a key does not need a remove first.
pub fn add(key_id: &str, pem: &[u8]) -> Result<PathBuf, String> {
    if !package::valid_key_id(key_id) {
        return Err(format!(
            "invalid keyId {:?}; expected [A-Za-z0-9._-]{{1,64}}",
            key_id
        ));
    }
    if pem.is_empty() {
        return Err("public key is empty".into());
    }
    if pem.len() as u64 > MAX_KEY_BYTES {
        return Err(format!("public key exceeds {} KiB", MAX_KEY_BYTES / 1024));
    }
    package::validate_public_key(pem)?;
    let directory = paths::trust_dir();
    fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {}", directory.display(), error))?;
    set_dir_mode(&directory);
    let target = key_path(key_id);
    fs::write(&target, pem).map_err(|error| format!("write {}: {}", target.display(), error))?;
    set_file_mode(&target);
    Ok(target)
}

/// Remove a trusted key; `Ok(false)` means it was not there.
pub fn remove(key_id: &str) -> Result<bool, String> {
    if !package::valid_key_id(key_id) {
        return Err(format!(
            "invalid keyId {:?}; expected [A-Za-z0-9._-]{{1,64}}",
            key_id
        ));
    }
    let path = key_path(key_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("remove {}: {}", path.display(), error)),
    }
}

#[cfg(unix)]
fn set_file_mode(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
}

#[cfg(not(unix))]
fn set_file_mode(_path: &std::path::Path) {}

#[cfg(unix)]
fn set_dir_mode(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &std::path::Path) {}
