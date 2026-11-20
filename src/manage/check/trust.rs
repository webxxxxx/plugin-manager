//! Trust roots for `.spk` signatures: `<config>/trust.d/<keyId>.pub`.
//!
//! A package can never carry its own trust root, so provisioning is an explicit
//! admin action: `svc trust add <file.pub>`, an upload in the web console, or
//! dropping a PEM into `trust.d`. Only Ed25519 SPKI public keys are accepted.
//!
//! Every failure names its [`Problem`] where it is raised (#151): the HTTP/CLI
//! projections read the category, never the detail text.

use std::fs;
use std::path::PathBuf;

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::platform::paths::Paths;

/// Public keys are tiny; a larger file is a mistake, not a key.
pub const MAX_KEY_BYTES: u64 = 16 * 1024;

/// Sorted key ids present in `trust.d`; a missing directory means "none".
pub fn keys(paths: &Paths) -> Result<Vec<String>, ProblemError> {
    let directory = &paths.trust_dir;
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Problem::Internal.error(format!("read {}: {}", directory.display(), error)))
        }
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

pub fn key_path(paths: &Paths, key_id: &str) -> PathBuf {
    paths.trust_dir.join(format!("{}.pub", key_id))
}

/// Validate and write `<keyId>.pub`; an existing file with the same id is
/// replaced so rotating a key does not need a remove first.
pub fn add(paths: &Paths, key_id: &str, pem: &[u8]) -> Result<PathBuf, ProblemError> {
    check_key_id(key_id)?;
    if pem.is_empty() {
        return Err(Problem::Invalid.error("public key is empty"));
    }
    if pem.len() as u64 > MAX_KEY_BYTES {
        return Err(
            Problem::TooLarge.error(format!("public key exceeds {} KiB", MAX_KEY_BYTES / 1024))
        );
    }
    package::validate_public_key(pem)?;
    let directory = &paths.trust_dir;
    fs::create_dir_all(directory).map_err(|error| {
        Problem::Internal.error(format!("create {}: {}", directory.display(), error))
    })?;
    set_dir_mode(directory);
    let target = key_path(paths, key_id);
    fs::write(&target, pem).map_err(|error| {
        Problem::Internal.error(format!("write {}: {}", target.display(), error))
    })?;
    set_file_mode(&target);
    Ok(target)
}

/// Remove a trusted key; `Ok(false)` means it was not there.
pub fn remove(paths: &Paths, key_id: &str) -> Result<bool, ProblemError> {
    check_key_id(key_id)?;
    let path = key_path(paths, key_id);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Problem::Internal.error(format!("remove {}: {}", path.display(), error))),
    }
}

fn check_key_id(key_id: &str) -> Result<(), ProblemError> {
    if package::valid_key_id(key_id) {
        Ok(())
    } else {
        Err(Problem::Invalid.error(format!(
            "invalid keyId {:?}; expected [A-Za-z0-9._-]{{1,64}}",
            key_id
        )))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> Paths {
        Paths::under(std::path::Path::new("/nonexistent-svcd-trust"))
    }

    #[test]
    fn a_bad_key_id_is_invalid_not_a_read_failure() {
        let error = add(&test_paths(), "../escape", b"pem").expect_err("path traversal");
        assert_eq!(error.problem, Problem::Invalid);
        assert!(error.detail().contains("invalid keyId"), "{}", error);

        let error = remove(&test_paths(), "../escape").expect_err("path traversal");
        assert_eq!(error.problem, Problem::Invalid);
    }

    #[test]
    fn an_oversized_key_is_too_large() {
        let error = add(
            &test_paths(),
            "demo",
            &vec![b'p'; MAX_KEY_BYTES as usize + 1],
        )
        .expect_err("oversized");
        assert_eq!(error.problem, Problem::TooLarge);
    }

    #[test]
    fn an_empty_key_is_invalid() {
        let error = add(&test_paths(), "demo", b"").expect_err("empty");
        assert_eq!(error.problem, Problem::Invalid);
    }
}
