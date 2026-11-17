use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::host::dirs;
use crate::settings::{self, Settings};

pub use crate::platform::host::dirs::user_mode;
pub(crate) use crate::platform::host::dirs::platform_config_dir;

/// Every directory the process uses, resolved once at the composition root.
///
/// The value is read-only by construction (no method re-reads the environment
/// or the loaded settings), so two subsystems cannot disagree about a path and
/// a test can build a whole tree without touching process state. Fields are
/// always absolute paths.
///
/// Two constructors only:
/// - [`Paths::resolve`] for production: platform default → `SVCD_*` → `svcd.conf`.
/// - [`Paths::under`] for tests: the same layout under a temporary root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub services_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub run_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub trust_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// Production resolution. Each directory follows the documented priority:
    /// a `SVCD_*` override wins, then the corresponding `svcd.conf` key, then
    /// the platform (or user-profile) default. `services_dir`, `plugins_dir`
    /// and `trust_dir` derive from the resolved config/data directory unless
    /// the config names them explicitly.
    pub fn resolve(settings: &Settings) -> Paths {
        Self::resolve_with(settings, |name| std::env::var_os(name))
    }

    /// The environment lookup is a parameter so the priority chain can be
    /// tested without mutating process state.
    fn resolve_with(settings: &Settings, env: impl Fn(&str) -> Option<OsString>) -> Paths {
        let config_dir = env("SVCD_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.config_dir.clone())
            .unwrap_or_else(dirs::default_config_dir);
        let data_dir = env("SVCD_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.install_data_dir())
            .unwrap_or_else(default_data_dir);
        let logs_dir = env("SVCD_LOG_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.logs_dir.clone())
            .or_else(|| settings.install_logs_dir())
            .unwrap_or_else(default_logs_dir);
        let services_dir = settings
            .services_dir
            .clone()
            .unwrap_or_else(|| config_dir.join("services.d"));
        let temp_dir = env("SVCD_TEMP_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.temp_dir.clone())
            .unwrap_or_else(|| data_dir.join("temp"));
        let run_dir = env("SVCD_RUN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("run"));
        let plugins_dir = settings
            .plugins_dir
            .clone()
            .unwrap_or_else(|| data_dir.join("plugins"));
        let trust_dir = env("SVCD_TRUST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_dir.join("trust.d"));
        Paths {
            config_dir,
            services_dir,
            temp_dir,
            run_dir,
            plugins_dir,
            trust_dir,
            logs_dir,
            data_dir,
        }
    }

    /// Test construction: the same relative layout as a system install, with
    /// no environment or settings input. `root/config` holds the config tree
    /// and `root/data` the data tree; logs live in `root/logs`.
    pub fn under(root: &Path) -> Paths {
        let config_dir = root.join("config");
        let data_dir = root.join("data");
        Paths {
            services_dir: config_dir.join("services.d"),
            trust_dir: config_dir.join("trust.d"),
            plugins_dir: data_dir.join("plugins"),
            temp_dir: data_dir.join("temp"),
            run_dir: data_dir.join("run"),
            logs_dir: root.join("logs"),
            config_dir,
            data_dir,
        }
    }
}

fn default_data_dir() -> PathBuf {
    if user_mode() {
        dirs::user_data_dir()
    } else {
        dirs::platform_data_dir()
    }
}

fn default_logs_dir() -> PathBuf {
    if user_mode() {
        dirs::user_logs_dir()
    } else {
        dirs::platform_logs_dir()
    }
}

/// The configuration directory: `svcd.conf`, `trust.d` and the local control
/// token. `SVCD_CONFIG_DIR` overrides it for tests; otherwise `config_dir` in
/// the loaded `svcd.conf` wins over the platform default.
/// Production defaults are `/etc/svcd`, `/Library/Application Support/svcd/config`
/// and `%ProgramData%\svcd\config`; a non-root process defaults to its user
/// profile (`%APPDATA%\svcd` on Windows).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = &settings.config_dir {
            return dir.clone();
        }
    }
    dirs::default_config_dir()
}

pub fn services_dir() -> PathBuf {
    settings::current()
        .ok()
        .and_then(|settings| settings.services_dir.clone())
        .unwrap_or_else(|| config_dir().join("services.d"))
}

/// Transient plugin data. `SVCD_TEMP_DIR` overrides it for tests/plugins;
/// `temp_dir` in `svcd.conf` wins over the default `<data>/temp`.
pub fn temp_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_TEMP_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = &settings.temp_dir {
            return dir.clone();
        }
    }
    data_dir().join("temp")
}

/// Per-service runtime records (`<data>/run/<id>.json`) that let a restarted
/// daemon reconnect to the processes that survived it. `SVCD_RUN_DIR` overrides
/// it for tests.
pub fn run_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_RUN_DIR") {
        return PathBuf::from(dir);
    }
    data_dir().join("run")
}

/// Installed plugins: `<data>/plugins/<id>/<version>/`, or `plugins_dir` from
/// `svcd.conf` when configured.
pub fn plugins_dir() -> PathBuf {
    // Unit tests share one process and may have loaded settings from another
    // test's config. The override is thread-local so one test's temp tree
    // cannot redirect a later test on another thread.
    #[cfg(test)]
    if let Some(dir) = TEST_PLUGINS_DIR.with(|slot| slot.borrow().clone()) {
        return dir;
    }
    settings::current()
        .ok()
        .and_then(|settings| settings.plugins_dir.clone())
        .unwrap_or_else(|| data_dir().join("plugins"))
}

#[cfg(test)]
thread_local! {
    static TEST_PLUGINS_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point the current test thread at its own plugin tree.
#[cfg(test)]
pub(crate) fn set_test_plugins_dir(dir: PathBuf) {
    TEST_PLUGINS_DIR.with(|slot| *slot.borrow_mut() = Some(dir));
}

/// Ed25519 trust roots: `<config>/trust.d/<keyId>.pub` (PEM SPKI).
pub fn trust_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_TRUST_DIR") {
        return PathBuf::from(dir);
    }
    config_dir().join("trust.d")
}

/// The platform log directory. `SVCD_LOG_DIR` overrides it for tests and
/// non-root development; `install_dir` moves it under the installation root
/// for read-only-rootfs hosts. Production paths are `/var/log/svcd`,
/// `/Library/Logs/svcd` and `%ProgramData%\svcd\logs`.
pub fn logs_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_LOG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = &settings.logs_dir {
            return dir.clone();
        }
        if let Some(dir) = settings.install_logs_dir() {
            return dir;
        }
    }
    if user_mode() {
        return dirs::user_logs_dir();
    }
    dirs::platform_logs_dir()
}

/// The platform data directory. `SVCD_DATA_DIR` overrides it so tests and
/// non-root development runs can use an isolated tree; `install_dir` moves it
/// under the installation root when `/var` is not persistent. Production paths
/// are `/var/lib/svcd`, `/Library/Application Support/svcd` and
/// `%ProgramData%\svcd`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SVCD_DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(settings) = settings::current() {
        if let Some(dir) = settings.install_data_dir() {
            return dir;
        }
    }
    if user_mode() {
        return dirs::user_data_dir();
    }
    dirs::platform_data_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment table used instead of the process environment, so the
    /// priority chain is testable in any order and on any host.
    struct FakeEnv(&'static [(&'static str, &'static str)]);

    impl FakeEnv {
        fn get(&self, name: &str) -> Option<OsString> {
            self.0
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn settings_overrides_are_used() -> Settings {
        Settings {
            config_dir: Some(PathBuf::from("/conf-from-file")),
            install_dir: Some(PathBuf::from("/install-from-file")),
            services_dir: Some(PathBuf::from("/services-from-file")),
            plugins_dir: Some(PathBuf::from("/plugins-from-file")),
            temp_dir: Some(PathBuf::from("/temp-from-file")),
            logs_dir: Some(PathBuf::from("/logs-from-file")),
            ..Settings::default()
        }
    }

    #[test]
    fn resolve_reads_environment_then_config_then_platform_defaults() {
        let settings = settings_overrides_are_used();
        let env = FakeEnv(&[
            ("SVCD_CONFIG_DIR", "/conf-from-env"),
            ("SVCD_DATA_DIR", "/data-from-env"),
            ("SVCD_LOG_DIR", "/logs-from-env"),
            ("SVCD_TEMP_DIR", "/temp-from-env"),
            ("SVCD_RUN_DIR", "/run-from-env"),
            ("SVCD_TRUST_DIR", "/trust-from-env"),
        ]);
        let paths = Paths::resolve_with(&settings, |name| env.get(name));
        assert_eq!(paths.config_dir, PathBuf::from("/conf-from-env"));
        assert_eq!(paths.data_dir, PathBuf::from("/data-from-env"));
        assert_eq!(paths.logs_dir, PathBuf::from("/logs-from-env"));
        assert_eq!(paths.temp_dir, PathBuf::from("/temp-from-env"));
        assert_eq!(paths.run_dir, PathBuf::from("/run-from-env"));
        assert_eq!(paths.trust_dir, PathBuf::from("/trust-from-env"));
        // No override exists for these: the config file decides.
        assert_eq!(paths.services_dir, PathBuf::from("/services-from-file"));
        assert_eq!(paths.plugins_dir, PathBuf::from("/plugins-from-file"));
    }

    #[test]
    fn resolve_uses_the_config_file_when_the_environment_is_silent() {
        let settings = settings_overrides_are_used();
        let paths = Paths::resolve_with(&settings, |_| None);
        assert_eq!(paths.config_dir, PathBuf::from("/conf-from-file"));
        // `install_dir` moves data and logs, and the file's explicit keys win.
        assert_eq!(paths.data_dir, PathBuf::from("/install-from-file/data"));
        assert_eq!(paths.logs_dir, PathBuf::from("/logs-from-file"));
        assert_eq!(paths.services_dir, PathBuf::from("/services-from-file"));
        assert_eq!(paths.plugins_dir, PathBuf::from("/plugins-from-file"));
        assert_eq!(paths.temp_dir, PathBuf::from("/temp-from-file"));
        // Unnamed directories derive from data/config.
        assert_eq!(paths.run_dir, PathBuf::from("/install-from-file/data/run"));
        assert_eq!(paths.trust_dir, PathBuf::from("/conf-from-file/trust.d"));
    }

    #[test]
    fn resolve_falls_back_to_the_platform_defaults() {
        let paths = Paths::resolve_with(&Settings::default(), |_| None);
        assert_eq!(paths.config_dir, dirs::default_config_dir());
        assert_eq!(
            paths.services_dir,
            paths.config_dir.join("services.d")
        );
        assert_eq!(paths.trust_dir, paths.config_dir.join("trust.d"));
        assert_eq!(paths.data_dir, default_data_dir());
        assert_eq!(paths.plugins_dir, paths.data_dir.join("plugins"));
        assert_eq!(paths.temp_dir, paths.data_dir.join("temp"));
        assert_eq!(paths.run_dir, paths.data_dir.join("run"));
        assert_eq!(paths.logs_dir, default_logs_dir());
    }

    #[test]
    fn under_mirrors_the_system_layout_under_a_temporary_root() {
        let root = Path::new("/tmp/svcd-test-root");
        let under = Paths::under(root);
        // The explicit relative structure: config/ and data/ trees plus logs/.
        assert_eq!(under.config_dir, root.join("config"));
        assert_eq!(under.services_dir, root.join("config/services.d"));
        assert_eq!(under.trust_dir, root.join("config/trust.d"));
        assert_eq!(under.data_dir, root.join("data"));
        assert_eq!(under.plugins_dir, root.join("data/plugins"));
        assert_eq!(under.temp_dir, root.join("data/temp"));
        assert_eq!(under.run_dir, root.join("data/run"));
        assert_eq!(under.logs_dir, root.join("logs"));

        // A config that pins this root resolves to exactly the same tree.
        let settings = Settings {
            config_dir: Some(under.config_dir.clone()),
            install_dir: Some(root.to_path_buf()),
            ..Settings::default()
        };
        assert_eq!(Paths::resolve_with(&settings, |_| None), under);
    }
}
