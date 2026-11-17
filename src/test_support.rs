//! Test-only filesystem roots.
//!
//! Unit tests run from `target/<profile>/deps`; a sibling `target/<profile>/tmp`
//! is always writable, while a restricted Windows `%TEMP%` ACL is not. Unix
//! keeps the short system temp because control-channel tests bind Unix domain
//! sockets, whose paths have a hard length limit.

use std::path::PathBuf;

pub(crate) fn temp_dir() -> PathBuf {
    if !cfg!(windows) {
        return std::env::temp_dir();
    }
    let base = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent()?.parent().map(|dir| dir.join("tmp")))
        .unwrap_or_else(std::env::temp_dir);
    let _ = std::fs::create_dir_all(&base);
    base
}
