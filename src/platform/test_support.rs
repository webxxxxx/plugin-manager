//! Platform-owned test values that need the platform layer.
//!
//! Keeping these here (rather than in a crate-root helper) means a platform
//! module never reaches above its own layer just to find a temp directory.

/// A writable root for tests that write files.
///
/// Unit tests run from `target/<profile>/deps`; a sibling `target/<profile>/tmp`
/// is always writable, while a restricted Windows `%TEMP%` ACL is not. Unix
/// keeps the short system temp because control-channel tests bind Unix domain
/// sockets, whose paths have a hard length limit.
pub(crate) fn temp_dir() -> std::path::PathBuf {
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

/// The one test `Limits` value: every module's wrapper delegates here so a
/// change to the test profile cannot drift between suites.
pub(crate) fn limits() -> std::sync::Arc<crate::platform::limits::Limits> {
    std::sync::Arc::new(crate::platform::limits::test_default())
}

/// A unique directory set for one test module. `prefix` keeps concurrent test
/// binaries from sharing a tree under the same temp root.
pub(crate) fn paths(prefix: &str) -> crate::platform::paths::Paths {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    crate::platform::paths::Paths::under(&temp_dir().join(format!(
        "svcd-{}-{}-{}",
        prefix,
        std::process::id(),
        nanos
    )))
}
