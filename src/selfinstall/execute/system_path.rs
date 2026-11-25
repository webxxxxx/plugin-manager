//! The machine-wide PATH entry a system install adds (#311).
//!
//! User mode edits the profile of whoever ran the installer (`user_path.rs`).
//! A system install is different: the aliases land in a prefix that is on no
//! one's PATH, so the *machine* has to learn it — the registry value on
//! Windows, `/etc/profile.d` on Linux, `/etc/paths.d` on macOS. `--manual`
//! stays out: its launcher is started by an init script and nobody types
//! `svc` from that machine's shell by hand.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

/// Debug seam: tests point the profile directory at their own tree instead of
/// `/etc`. Windows uses [`test_key`] for the same reason.
#[cfg(all(debug_assertions, unix))]
pub(super) fn test_base() -> Option<PathBuf> {
    std::env::var_os("SVCD_TEST_SYSTEM_ROOT").map(PathBuf::from)
}

#[cfg(all(not(debug_assertions), unix))]
pub(super) fn test_base() -> Option<PathBuf> {
    None
}

/// Debug seam: skip the edit entirely. Tests that only assert the installed
/// files set this so a developer's machine PATH is never touched.
#[cfg(debug_assertions)]
pub(super) fn skipped() -> bool {
    std::env::var_os("SVCD_TEST_NO_SYSTEM_PATH").is_some()
}

#[cfg(not(debug_assertions))]
pub(super) fn skipped() -> bool {
    false
}

/// The profile directory the POSIX hosts read: `/etc`, or the test override.
#[cfg(unix)]
pub(super) fn base() -> PathBuf {
    test_base().unwrap_or_else(|| PathBuf::from("/etc"))
}

/// Add `dir` to the machine PATH. The returned paths are the files that changed.
#[cfg(unix)]
pub(super) fn add_system_path(base: &Path, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let file = system_path_file(base, dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {}", parent.display(), error))?;
    }
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let directory = dir.display().to_string();
    let block = if cfg!(target_os = "macos") {
        format!("{}\n", directory)
    } else {
        format!(
            "# svcd system path\ncase \":$PATH:\" in\n  *\":{}\"*) ;;\n  *) PATH=\"{}:$PATH\" ;;\nesac\nexport PATH\n",
            directory, directory
        )
    };
    if text.contains(&block) {
        return Ok(Vec::new());
    }
    let mut updated = text;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(&block);
    std::fs::write(&file, updated)
        .map_err(|error| format!("write {}: {}", file.display(), error))?;
    Ok(vec![file])
}

/// Remove the machine PATH entry. The returned paths are the files that changed.
#[cfg(unix)]
pub(super) fn remove_system_path(base: &Path, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let file = system_path_file(base, dir);
    if !file.is_file() {
        return Ok(Vec::new());
    }
    // The file is ours alone, so deleting it removes the entry without having
    // to parse the operator's shell syntax back apart.
    std::fs::remove_file(&file).map_err(|error| format!("remove {}: {}", file.display(), error))?;
    Ok(vec![file])
}

#[cfg(unix)]
fn system_path_file(base: &Path, _dir: &Path) -> PathBuf {
    // One file per host, not per directory: a second install with another prefix
    // rewrites the same entry, which is what a single served daemon means.
    if cfg!(target_os = "macos") {
        base.join("paths.d").join("svcd")
    } else {
        base.join("profile.d").join("svcd.sh")
    }
}

/// The machine environment key: `HKLM\SYSTEM\…\Environment`, or the test
/// override (`HKCU:Software\…`).
#[cfg(windows)]
pub(super) fn machine_key() -> Option<(super::path_env::Hive, String)> {
    use super::path_env::Hive;

    if let Some(value) = std::env::var_os("SVCD_TEST_SYSTEM_PATH_KEY") {
        let value = value.to_string_lossy().to_string();
        let (hive, subkey) = match value.split_once(':') {
            Some(("HKCU", subkey)) => (Hive::CurrentUser, subkey.to_string()),
            Some(("HKLM", subkey)) => (Hive::LocalMachine, subkey.to_string()),
            _ => return None,
        };
        return Some((hive, subkey));
    }
    Some((
        Hive::LocalMachine,
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment".to_string(),
    ))
}

/// Add `dir` to the machine PATH. `true` when the value changed.
#[cfg(windows)]
pub(super) fn add_system_path(
    hive: super::path_env::Hive,
    subkey: &str,
    dir: &Path,
) -> Result<bool, String> {
    let directory = dir.display().to_string();
    let existing = super::path_env::read(hive, subkey)?.unwrap_or_default();
    let (updated, changed) = super::path_env::add_entry(&existing, &directory);
    if !changed {
        return Ok(false);
    }
    super::path_env::write(hive, subkey, &updated)?;
    super::path_env::broadcast_change();
    Ok(true)
}

/// Remove `dir` from the machine PATH. `true` when the value changed.
#[cfg(windows)]
pub(super) fn remove_system_path(
    hive: super::path_env::Hive,
    subkey: &str,
    dir: &Path,
) -> Result<bool, String> {
    let directory = dir.display().to_string();
    let existing = super::path_env::read(hive, subkey)?.unwrap_or_default();
    let (updated, changed) = super::path_env::remove_entry(&existing, &directory);
    if !changed {
        return Ok(false);
    }
    super::path_env::write(hive, subkey, &updated)?;
    super::path_env::broadcast_change();
    Ok(true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_system_path_file_holds_one_entry_and_is_idempotent() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-system-path");
        let base = temp.path();
        let dir = Path::new("/opt/svcd");

        let written = add_system_path(base, dir).expect("add");
        assert_eq!(written.len(), 1, "one profile file");
        let file = system_path_file(base, dir);
        let text = std::fs::read_to_string(&file).expect("read");
        assert!(text.contains("/opt/svcd"), "{text}");
        if !cfg!(target_os = "macos") {
            assert!(text.contains("case \":$PATH:\""), "{text}");
        }

        // A reinstall must not append the entry twice.
        let again = add_system_path(base, dir).expect("add again");
        assert!(again.is_empty(), "nothing changed the second time");
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            text,
            "the file is byte-identical"
        );

        let removed = remove_system_path(base, dir).expect("remove");
        assert_eq!(removed, vec![file.clone()]);
        assert!(!file.exists(), "the entry's file is gone");
        assert!(remove_system_path(base, dir)
            .expect("remove again")
            .is_empty());
    }
}
