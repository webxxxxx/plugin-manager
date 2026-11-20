//! Platform-owned test values that need the platform layer.
//!
//! Keeping these here (rather than in the crate-root test_support) avoids a
//! test-only cycle: platform modules import this module, and the root helper
//! only depends on std.

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
    crate::platform::paths::Paths::under(&crate::test_support::temp_dir().join(format!(
        "svcd-{}-{}-{}",
        prefix,
        std::process::id(),
        nanos
    )))
}
