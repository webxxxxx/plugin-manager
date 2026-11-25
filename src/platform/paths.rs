use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::host::dirs;
use crate::platform::settings::Settings;

pub use crate::platform::host::dirs::user_mode;
pub(crate) use crate::platform::host::dirs::{normalize_text, platform_config_dir};

/// [`normalize_text`] for a value that is already a path.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    PathBuf::from(normalize_text(&path.to_string_lossy()))
}

/// Every directory the process uses, resolved once at the composition root.
///
/// Three roots are configurable: `config_dir`, `data_dir`, `logs_dir`.
/// Everything else derives from one of them. `install_dir` is only an install
/// prefix: the bootstrap folds it into the roots before this value is built, so
/// the resolver never re-derives runtime paths from it (ADR-0023).
///
/// The value is read-only by construction (no method re-reads the environment
/// or the loaded settings), so two subsystems cannot disagree about a path and
/// a test can build a whole tree without touching process state. Fields are
/// always absolute paths.
///
/// Two constructors:
/// - [`Paths::resolve`] for production: `SVCD_*` overrides, then the
///   materialized roots, then the platform defaults.
/// - [`Paths::under`] for tests: the same layout under a temporary root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub services_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub run_dir: PathBuf,
    /// Plugin roots in priority order, never empty. The first root is the
    /// default install target; a plugin may live on any of them, which is why
    /// its own directory is resolved through `plugin-locations.json`
    /// (ADR-0030) instead of being guessed from this list.
    pub plugin_roots: Vec<PathBuf>,
    pub trust_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// Production resolution. A `SVCD_*` root override wins, then the
    /// materialized root in `svcd.conf`, then the platform (or user-profile)
    /// default. Every derived directory follows its root.
    pub fn resolve(settings: &Settings) -> Paths {
        Self::resolve_with(settings, |name| std::env::var_os(name))
    }

    /// The environment lookup is a parameter so the priority chain can be
    /// tested without mutating process state. The installer also calls this
    /// with a clean boot environment (only `SVCD_CONFIG_DIR`), so install and
    /// daemon share one algorithm without inheriting the operator's shell.
    pub(crate) fn resolve_with(
        settings: &Settings,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Paths {
        Self::resolve_reporting(settings, env).0
    }

    /// The same value plus the warnings for removed keys and variables.
    pub(crate) fn resolve_reporting(
        settings: &Settings,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> (Paths, Vec<String>) {
        let config_dir = env("SVCD_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.config_dir.clone())
            .unwrap_or_else(dirs::default_config_dir);
        let data_dir = env("SVCD_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.data_dir.clone())
            .unwrap_or_else(dirs::default_data_dir);
        let logs_dir = env("SVCD_LOG_DIR")
            .map(PathBuf::from)
            .or_else(|| settings.logs_dir.clone())
            .unwrap_or_else(dirs::default_logs_dir);
        let services_dir = config_dir.join("services.d");
        let temp_dir = data_dir.join("temp");
        let run_dir = data_dir.join("run");
        // ADR-0023 derived this from `data_dir`; ADR-0030 lets a document list
        // more roots, in priority order. An unset list is exactly the old
        // single-root layout.
        let plugin_roots = if settings.plugins_dir.is_empty() {
            vec![data_dir.join("plugins")]
        } else {
            settings.plugins_dir.clone()
        };
        let trust_dir = config_dir.join("trust.d");
        let paths = Paths {
            config_dir,
            services_dir,
            temp_dir,
            run_dir,
            plugin_roots,
            trust_dir,
            logs_dir,
            data_dir,
        };
        let mut warnings = Vec::new();
        for ignored in &settings.ignored_path_keys {
            let actual = match ignored.key {
                "services_dir" => &paths.services_dir,
                _ => &paths.temp_dir,
            };
            warnings.push(format!(
                "svcd.conf: {} = {} is ignored; using {}",
                ignored.key,
                ignored.value.display(),
                actual.display()
            ));
        }
        // A multi-root list may omit the directory ADR-0023 used, and then the
        // plugins living there would silently stop being discovered. Say it
        // once at boot with the count; never move or delete anything.
        let legacy = paths.data_dir.join("plugins");
        if !paths.plugin_roots.contains(&legacy) {
            let installed = crate::platform::locations::installed_in(&legacy).len();
            if installed > 0 {
                warnings.push(format!(
                    "svcd.conf: plugins_dir does not include {}; {} installed plugin(s) there will not be discovered; add it to the list or move them",
                    legacy.display(),
                    installed
                ));
            }
        }
        // A plugin root on a volume that is not mounted keeps its index
        // entries (nobody loses a plugin because a card is out), but those
        // plugins are hidden from the list. Say which ones, once per boot.
        let hidden = crate::platform::locations::hidden(&paths);
        if !hidden.is_empty() {
            let rows: Vec<String> = hidden
                .iter()
                .map(|(id, root)| format!("{} ({})", id, root.display()))
                .collect();
            warnings.push(format!(
                "plugin root not available; {} plugin(s) stay hidden until the volume is back: {}",
                hidden.len(),
                rows.join(", ")
            ));
        }
        for (variable, actual) in [
            ("SVCD_TEMP_DIR", &paths.temp_dir),
            ("SVCD_RUN_DIR", &paths.run_dir),
            ("SVCD_TRUST_DIR", &paths.trust_dir),
        ] {
            if env(variable).is_some() {
                warnings.push(format!(
                    "{} is ignored; using {}",
                    variable,
                    actual.display()
                ));
            }
        }
        (paths, warnings)
    }

    /// The directory the control channel binds. Config is input only; Unix
    /// sockets live under `data/run`. Windows named pipes have no filesystem
    /// entry and stay keyed by the config directory, which is also the tier
    /// that decides their ACL.
    pub fn control_dir(&self) -> &Path {
        #[cfg(windows)]
        {
            &self.config_dir
        }
        #[cfg(not(windows))]
        {
            &self.run_dir
        }
    }

    /// The root a new plugin goes to when nothing else decides. Root selection
    /// by free space (ADR-0030) starts from the whole list, not from this.
    pub fn default_plugin_root(&self) -> &Path {
        // The constructor never builds an empty list; the index is `[0]`.
        &self.plugin_roots[0]
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
            plugin_roots: vec![data_dir.join("plugins")],
            temp_dir: data_dir.join("temp"),
            run_dir: data_dir.join("run"),
            logs_dir: root.join("logs"),
            config_dir,
            data_dir,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::settings::IgnoredPathKey;

    struct FakeEnv(&'static [(&'static str, &'static str)]);

    impl FakeEnv {
        fn get(&self, name: &str) -> Option<OsString> {
            self.0
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(*value))
        }
    }

    fn settings_with_roots() -> Settings {
        Settings {
            config_dir: Some(PathBuf::from("/conf-from-file")),
            data_dir: Some(PathBuf::from("/data-from-file")),
            logs_dir: Some(PathBuf::from("/logs-from-file")),
            install_dir: Some(PathBuf::from("/install-from-file")),
            ..Settings::default()
        }
    }

    #[test]
    fn resolve_reads_environment_then_config_roots() {
        let settings = settings_with_roots();
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
        assert_eq!(paths.services_dir, paths.config_dir.join("services.d"));
        assert_eq!(paths.trust_dir, paths.config_dir.join("trust.d"));
        assert_eq!(paths.plugin_roots, vec![paths.data_dir.join("plugins")]);
        assert_eq!(paths.temp_dir, paths.data_dir.join("temp"));
        assert_eq!(paths.run_dir, paths.data_dir.join("run"));
    }

    #[test]
    fn resolve_uses_the_materialized_roots_when_the_environment_is_silent() {
        let settings = settings_with_roots();
        let paths = Paths::resolve_with(&settings, |_| None);
        assert_eq!(paths.config_dir, PathBuf::from("/conf-from-file"));
        assert_eq!(paths.data_dir, PathBuf::from("/data-from-file"));
        assert_eq!(paths.logs_dir, PathBuf::from("/logs-from-file"));
        assert_eq!(
            paths.plugin_roots,
            vec![PathBuf::from("/data-from-file/plugins")]
        );
        assert_eq!(paths.run_dir, PathBuf::from("/data-from-file/run"));
        assert_eq!(paths.temp_dir, PathBuf::from("/data-from-file/temp"));
        assert_eq!(paths.trust_dir, PathBuf::from("/conf-from-file/trust.d"));
    }

    #[test]
    fn install_dir_never_moves_a_runtime_root() {
        let settings = Settings {
            install_dir: Some(PathBuf::from("/install-from-file")),
            ..Settings::default()
        };
        let paths = Paths::resolve_with(&settings, |_| None);
        assert_eq!(paths.config_dir, dirs::default_config_dir());
        assert_eq!(paths.data_dir, dirs::default_data_dir());
        assert_eq!(paths.logs_dir, dirs::default_logs_dir());
    }

    #[test]
    fn resolve_falls_back_to_the_platform_defaults() {
        let paths = Paths::resolve_with(&Settings::default(), |_| None);
        assert_eq!(paths.config_dir, dirs::default_config_dir());
        assert_eq!(paths.services_dir, paths.config_dir.join("services.d"));
        assert_eq!(paths.trust_dir, paths.config_dir.join("trust.d"));
        assert_eq!(paths.data_dir, dirs::default_data_dir());
        assert_eq!(paths.plugin_roots, vec![paths.data_dir.join("plugins")]);
        assert_eq!(paths.temp_dir, paths.data_dir.join("temp"));
        assert_eq!(paths.run_dir, paths.data_dir.join("run"));
        assert_eq!(paths.logs_dir, dirs::default_logs_dir());
    }

    #[test]
    fn removed_keys_and_variables_warn_with_the_derived_path() {
        let settings = Settings {
            config_dir: Some(PathBuf::from("/conf")),
            data_dir: Some(PathBuf::from("/data")),
            ignored_path_keys: vec![
                IgnoredPathKey {
                    key: "services_dir",
                    value: PathBuf::from("/old/services"),
                },
                IgnoredPathKey {
                    key: "temp_dir",
                    value: PathBuf::from("/old/temp"),
                },
            ],
            ..Settings::default()
        };
        let env = FakeEnv(&[
            ("SVCD_TEMP_DIR", "/env/temp"),
            ("SVCD_RUN_DIR", "/env/run"),
            ("SVCD_TRUST_DIR", "/env/trust"),
        ]);
        let (paths, warnings) = Paths::resolve_reporting(&settings, |name| env.get(name));
        assert_eq!(paths.plugin_roots, vec![PathBuf::from("/data/plugins")]);
        assert_eq!(warnings.len(), 5, "{warnings:?}");
        for warning in &warnings {
            assert!(warning.contains("ignored"), "{warning}");
        }
        let actual = [
            paths.services_dir.display().to_string(),
            paths.temp_dir.display().to_string(),
            paths.run_dir.display().to_string(),
            paths.trust_dir.display().to_string(),
        ];
        for path in actual {
            assert!(
                warnings.iter().any(|warning| warning.contains(&path)),
                "no warning names {path}: {warnings:?}"
            );
        }
    }

    #[test]
    fn a_plugin_root_list_keeps_its_order_and_its_first_root_is_the_default() {
        let settings = Settings {
            plugins_dir: vec![
                PathBuf::from("/mnt/sd/svcd/plugins"),
                PathBuf::from("/data/plugins"),
            ],
            data_dir: Some(PathBuf::from("/data")),
            ..Settings::default()
        };
        let paths = Paths::resolve_with(&settings, |_| None);
        assert_eq!(
            paths.plugin_roots,
            vec![
                PathBuf::from("/mnt/sd/svcd/plugins"),
                PathBuf::from("/data/plugins")
            ]
        );
        assert_eq!(
            paths.default_plugin_root(),
            Path::new("/mnt/sd/svcd/plugins")
        );
    }

    /// A list that omits the ADR-0023 directory must not make the plugins
    /// there disappear quietly: the boot warning names the directory and the
    /// count, and nothing on disk is touched.
    #[test]
    fn dropping_the_derived_root_from_the_list_warns_about_the_plugins_left_there() {
        let root = crate::platform::test_support::TempDir::new();
        let data_dir = root.0.join("data");
        for id in ["alpha", "beta"] {
            let version = data_dir.join("plugins").join(id);
            std::fs::create_dir_all(&version).expect("plugin dir");
            std::fs::write(version.join("current"), "1.0.0").expect("current");
        }
        let settings = Settings {
            data_dir: Some(data_dir.clone()),
            plugins_dir: vec![PathBuf::from("/mnt/sd/svcd/plugins")],
            ..Settings::default()
        };
        let (paths, warnings) = Paths::resolve_reporting(&settings, |_| None);
        assert_eq!(
            paths.plugin_roots,
            vec![PathBuf::from("/mnt/sd/svcd/plugins")]
        );
        let warning = warnings
            .iter()
            .find(|warning| warning.contains(&data_dir.join("plugins").display().to_string()))
            .unwrap_or_else(|| panic!("no warning names the dropped root: {warnings:?}"));
        assert!(warning.contains('2'), "{warning}");

        // The same list plus the derived root is quiet: nothing is stranded.
        let settings = Settings {
            data_dir: Some(data_dir.clone()),
            plugins_dir: vec![
                PathBuf::from("/mnt/sd/svcd/plugins"),
                data_dir.join("plugins"),
            ],
            ..Settings::default()
        };
        let (_, warnings) = Paths::resolve_reporting(&settings, |_| None);
        assert!(
            !warnings
                .iter()
                .any(|warning| warning.contains("will not be discovered")),
            "{warnings:?}"
        );
    }

    #[test]
    fn under_mirrors_the_system_layout_under_a_temporary_root() {
        let root = Path::new("/tmp/svcd-test-root");
        let under = Paths::under(root);
        assert_eq!(under.config_dir, root.join("config"));
        assert_eq!(under.services_dir, root.join("config/services.d"));
        assert_eq!(under.trust_dir, root.join("config/trust.d"));
        assert_eq!(under.data_dir, root.join("data"));
        assert_eq!(under.plugin_roots, vec![root.join("data/plugins")]);
        assert_eq!(under.temp_dir, root.join("data/temp"));
        assert_eq!(under.run_dir, root.join("data/run"));
        assert_eq!(under.logs_dir, root.join("logs"));
    }

    #[test]
    fn control_dir_is_data_run_on_unix_and_config_on_windows() {
        let paths = Paths::under(Path::new("/tmp/svcd-control"));
        if cfg!(windows) {
            assert_eq!(paths.control_dir(), paths.config_dir);
        } else {
            assert_eq!(paths.control_dir(), paths.run_dir);
        }
    }
}
