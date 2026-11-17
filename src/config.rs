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

/// Everything the process needs to run, derived from one `svcd.conf`.
#[derive(Clone, Debug)]
pub struct Config {
    /// The document this process runs with.
    pub settings: Arc<Settings>,
    /// The file it was loaded from; `None` when no `svcd.conf` was located.
    pub settings_path: Option<PathBuf>,
    /// Every directory the process uses.
    pub paths: Arc<Paths>,
    /// Limits derived from `settings`.
    pub limits: Limits,
}

impl Config {
    /// Derive a configuration from an already-parsed document.
    ///
    /// Production goes through [`Config::bootstrap`]; unit tests that need a
    /// whole configuration use this with [`Paths::under`] instead of the
    /// process environment.
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
            limits,
        }
    }

    /// The single bootstrap point.
    ///
    /// Loads the settings document once (through the shared settings cache, so
    /// `--conf` and the default discovery cannot disagree), then resolves the
    /// directory set and the limits from it. A broken or unreadable document is
    /// a hard error: nothing silently falls back to defaults.
    pub fn bootstrap(conf: Option<&Path>) -> Result<Config, String> {
        if let Some(path) = conf {
            settings::init_from_path(path)?;
        }
        let settings = settings::current()?.clone();
        let settings_path = settings::path().map(Path::to_path_buf);
        Ok(Config::new(settings, settings_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_config_keeps_the_document_the_path_and_the_derived_limits() {
        let mut settings = Settings::default();
        settings.http_idle_timeout_secs = Some(7);
        settings.config_dir = Some(PathBuf::from("/etc/svcd"));
        let config = Config::new(settings, Some(PathBuf::from("/etc/svcd/svcd.conf")));

        assert_eq!(
            config.settings_path.as_deref(),
            Some(Path::new("/etc/svcd/svcd.conf"))
        );
        assert_eq!(config.settings.http_idle_timeout_secs, Some(7));
        // The limits are a pure projection of the document, not a second cache.
        assert_eq!(config.limits.idle_timeout, Duration::from_secs(7));
    }
}
