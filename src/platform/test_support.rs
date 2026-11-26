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

/// A directory set under `root` whose plugin roots are exactly `roots`.
///
/// `Paths::under` derives the other names; creating `data_dir` is what the
/// install and location tests need before they can write an index (#326).
pub(crate) fn paths_with_roots(
    root: &std::path::Path,
    roots: Vec<std::path::PathBuf>,
) -> crate::platform::paths::Paths {
    let mut paths = crate::platform::paths::Paths::under(root);
    paths.plugin_roots = roots;
    std::fs::create_dir_all(&paths.data_dir).expect("data dir");
    paths
}

/// A `Config` on a fresh tree named with `tag`, using default settings.
///
/// The tree is deliberately not created: the tests that need files create the
/// directories they touch, and nothing here reads the process environment.
pub(crate) fn config(tag: &str) -> std::sync::Arc<crate::platform::config::Config> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root = temp_dir().join(format!(
        "svcd-{}-{}-{}-{}",
        tag,
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::sync::Arc::new(crate::platform::config::Config::with_paths(
        crate::platform::settings::Settings::default(),
        None,
        crate::platform::paths::Paths::under(&root),
    ))
}

/// A unique directory removed on drop. `prefix` names the test module so a
/// leaked directory still tells you where it came from.
pub(crate) struct TempDir(pub(crate) std::path::PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        Self::with_prefix("test")
    }

    pub(crate) fn with_prefix(prefix: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = temp_dir().join(format!(
            "svcd-{}-{}-{}-{}",
            prefix,
            std::process::id(),
            nanos,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
