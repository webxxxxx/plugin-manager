//! Runtime configuration, derived once at the composition root.
//!
//! [`Config`] is the read-only value the process hands to every subsystem: the
//! parsed `svcd.conf`, the file it came from, the directory set and the limits
//! derived from the document. The `SVCD_*` overrides and the file discovery are
//! read here — once — and modules below the composition root receive their
//! directories as parameters instead of consulting process state.
//!
//! Shape and rationale:
//! [`docs/adr/0019-config-is-derived-once-at-the-composition-root.md`](../../docs/adr/0019-config-is-derived-once-at-the-composition-root.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::limits::{self, Limits};
use crate::paths::Paths;
use crate::settings::{self, Settings};

/// Everything the composition root derives from one `svcd.conf`: the parsed
/// document, the file it came from, the directory set, and the limits.
///
/// The four fields are the bootstrap snapshot. There is no process-wide cache
/// behind any of them: every reader receives the value through a construction
/// parameter, so a unit test can supply its own document without touching the
/// process environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// The document this process runs with.
    pub settings: Arc<Settings>,
    /// The file it was loaded from; `None` when no `svcd.conf` was located.
    pub settings_path: Option<PathBuf>,
    /// Every directory the process uses.
    pub paths: Arc<Paths>,
    /// Limits derived from `settings`.
    pub limits: Arc<Limits>,
}

impl Config {
    /// Derive a configuration from an already-parsed document, resolving the
    /// directory set with [`Paths::resolve`] (the production path).
    ///
    /// Unit tests that need a whole configuration use [`Config::with_paths`]
    /// with [`Paths::under`] so they own the tree and never read the process
    /// environment.
    pub fn new(settings: Settings, settings_path: Option<PathBuf>) -> Config {
        let paths = Paths::resolve(&settings);
        Self::with_paths(settings, settings_path, paths)
    }

    /// The same value with an explicit directory set. Production derives it
    /// from the document; a unit test passes [`Paths::under`] and owns the
    /// whole tree without touching the process environment.
    pub fn with_paths(settings: Settings, settings_path: Option<PathBuf>, paths: Paths) -> Config {
        let limits = limits::derive(&settings);
        Config {
            paths: Arc::new(paths),
            settings: Arc::new(settings),
            settings_path,
            limits: Arc::new(limits),
        }
    }

    /// The single bootstrap point.
    ///
    /// Resolves and parses the settings document exactly once, then derives the
    /// directory set and the limits from it. There is no process cache behind
    /// the returned value: a broken or unreadable document is a hard error, and
    /// callers that find a missing discovered file get an empty document, not a
    /// silent retry.
    pub fn bootstrap(conf: Option<&Path>) -> Result<Config, String> {
        let (settings, settings_path) = settings::load(conf)?;
        Ok(Config::new(settings, settings_path))
    }

    /// [`Config::bootstrap`] for a process entry that reports the failure with
    /// its own prefix and exit code.
    pub fn bootstrap_or_report(label: &str) -> Option<Config> {
        match Self::bootstrap(None) {
            Ok(config) => Some(config),
            Err(error) => {
                eprintln!("{}: invalid config: {}", label, error);
                None
            }
        }
    }
}

/// Tolerant directory resolution for the few phases that must outlive a broken
/// config: the daemon's pre-`ensure_conf` directory, `stop`/`shutdown`, install
/// and uninstall, and the Windows SCM readiness probe.
///
/// It reads the same `SVCD_*` overrides as bootstrap but falls back to the
/// platform defaults instead of failing. `conf` is loaded when it exists; a
/// missing or broken document falls back to the default settings. [`Config::bootstrap`]
/// stays the hard error entry; this is the only other place that reads those
/// variables.
pub(crate) fn ambient_paths(conf: Option<&Path>) -> Paths {
    match Config::bootstrap(conf) {
        Ok(config) => config.paths.as_ref().clone(),
        Err(_) => Paths::resolve(&Settings::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn write_conf(name: &str, text: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = crate::test_support::temp_dir().join(format!(
            "svcd-config-{}-{}-{}",
            std::process::id(),
            nanos,
            name
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("svcd.conf");
        std::fs::write(&path, text).expect("write conf");
        path
    }

    #[test]
    fn a_config_keeps_the_document_the_path_and_the_derived_limits() {
        let mut settings = Settings::default();
        settings.http_idle_timeout_secs = Some(7);
        settings.config_dir = Some(PathBuf::from("/etc/svcd"));
        let config = Config::with_paths(
            settings,
            Some(PathBuf::from("/etc/svcd/svcd.conf")),
            Paths::under(Path::new("/tmp/svcd-config-unit")),
        );

        assert_eq!(
            config.settings_path.as_deref(),
            Some(Path::new("/etc/svcd/svcd.conf"))
        );
        assert_eq!(config.settings.http_idle_timeout_secs, Some(7));
        // The limits are a pure projection of the document, not a second cache.
        assert_eq!(config.limits.idle_timeout, Duration::from_secs(7));
    }

    #[test]
    fn bootstrap_reads_the_explicit_document_and_keeps_its_path() {
        let path = write_conf("explicit", "http_idle_timeout_secs = 7\n");
        let config = Config::bootstrap(Some(&path)).expect("bootstrap");

        assert_eq!(config.settings.http_idle_timeout_secs, Some(7));
        assert_eq!(config.settings_path.as_deref(), Some(path.as_path()));
        assert_eq!(config.limits.idle_timeout, Duration::from_secs(7));
    }

    #[test]
    fn two_bootstraps_from_different_documents_do_not_share_state() {
        // The old process-wide cache made the second document invisible. The
        // derived value must follow the document each call is given.
        let first = write_conf("first", "http_idle_timeout_secs = 7\n");
        let second = write_conf("second", "http_idle_timeout_secs = 11\n");

        let first = Config::bootstrap(Some(&first)).expect("first");
        let second = Config::bootstrap(Some(&second)).expect("second");

        assert_eq!(first.limits.idle_timeout, Duration::from_secs(7));
        assert_eq!(second.limits.idle_timeout, Duration::from_secs(11));
    }

    #[test]
    fn ambient_paths_honours_an_explicit_document() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-config-ambient-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let conf_dir = root.join("custom-config");
        let path = write_conf("ambient", &format!("config_dir = {}\n", conf_dir.display()));
        std::fs::create_dir_all(&conf_dir).expect("custom config dir");

        let paths = ambient_paths(Some(&path));
        assert_eq!(paths.config_dir, conf_dir);
    }

    #[test]
    fn a_missing_explicit_document_is_a_hard_error() {
        let path = crate::test_support::temp_dir().join("svcd-config-missing.conf");
        let error = Config::bootstrap(Some(&path)).expect_err("missing conf");
        assert!(error.contains("config not found"), "{error}");
    }
}
