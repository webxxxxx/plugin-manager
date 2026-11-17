use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::host::dirs;
use crate::platform::settings::Settings;

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
    #[cfg_attr(not(test), allow(dead_code))]
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
    fn install_dir_moves_logs_unless_the_environment_overrides_it() {
        let settings = Settings {
            install_dir: Some(PathBuf::from("/install-from-file")),
            ..Settings::default()
        };

        let paths = Paths::resolve_with(&settings, |_| None);
        assert_eq!(paths.data_dir, PathBuf::from("/install-from-file/data"));
        assert_eq!(paths.logs_dir, PathBuf::from("/install-from-file/logs"));

        // `SVCD_LOG_DIR` still wins over `install_dir`; that is the runtime
        // contract the launcher relies on.
        let paths = Paths::resolve_with(&settings, |name| {
            (name == "SVCD_LOG_DIR").then(|| OsString::from("/logs-from-env"))
        });
        assert_eq!(paths.logs_dir, PathBuf::from("/logs-from-env"));
        assert_eq!(paths.data_dir, PathBuf::from("/install-from-file/data"));
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
