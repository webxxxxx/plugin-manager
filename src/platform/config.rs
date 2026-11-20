//! Runtime configuration, derived once at the composition root.
//!
//! [`Config`] is the read-only value the process hands to every subsystem: the
//! parsed `svcd.conf`, the file it came from, the directory set, the limits
//! derived from the document, and the report that explains the derivation (the
//! memory budget, the workload shape and every rewritten value).
//!
//! The machine is probed exactly here: [`Config::bootstrap`] reads the host
//! once, [`Config::with_paths`] takes no probe and derives from the
//! compile-time ceiling instead, which is what keeps a test independent of the
//! machine it runs on.
//!
//! Shape and rationale:
//! [`docs/adr/0019-config-is-derived-once-at-the-composition-root.md`](../../docs/adr/0019-config-is-derived-once-at-the-composition-root.md)
//! and [`docs/adr/0026-the-memory-budget-is-the-truth.md`](../../docs/adr/0026-the-memory-budget-is-the-truth.md).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::platform::limits::{self, Derived, Limits, ResourceReport};
use crate::platform::paths::Paths;
use crate::platform::resources::{self, Machine, ProbePaths, ResourceProfile};
use crate::platform::settings::{self, Settings};

/// Everything the composition root derives from one `svcd.conf`: the parsed
/// document, the file it came from, the directory set, the limits and the
/// report behind them.
///
/// There is no process-wide cache behind any of them: every reader receives the
/// value through a construction parameter, so a unit test can supply its own
/// document without touching the process environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// The document this process runs with.
    pub settings: Arc<Settings>,
    /// The file it was loaded from; `None` when no `svcd.conf` was located.
    pub settings_path: Option<PathBuf>,
    /// Every directory the process uses.
    pub paths: Arc<Paths>,
    /// Limits derived from `settings` and the probed machine.
    pub limits: Arc<Limits>,
    /// Bytes, shape and rewrites behind `limits`.
    pub resources: Arc<ResourceReport>,
}

impl Config {
    /// The same value with an explicit directory set and no machine probe.
    ///
    /// Only tests and unit-test composition roots need this: they pass
    /// [`Paths::under`] and own the whole tree, and the empty profile keeps the
    /// derived limits the same on every machine (the budget is the build
    /// ceiling). Production goes through [`Config::bootstrap`].
    #[cfg(test)]
    pub fn with_paths(settings: Settings, settings_path: Option<PathBuf>, paths: Paths) -> Config {
        Self::assemble(settings, settings_path, paths, &ResourceProfile::default())
    }

    /// The same value with an explicit resource description. A test that wants
    /// to pin the machine states it here instead of probing this host.
    pub fn with_resources(
        settings: Settings,
        settings_path: Option<PathBuf>,
        paths: Paths,
        profile: &ResourceProfile,
    ) -> Config {
        Self::assemble(settings, settings_path, paths, profile)
    }

    fn assemble(
        settings: Settings,
        settings_path: Option<PathBuf>,
        paths: Paths,
        profile: &ResourceProfile,
    ) -> Config {
        let Derived { limits, report } = limits::derive_with(&settings, profile);
        Config {
            paths: Arc::new(paths),
            settings: Arc::new(settings),
            settings_path,
            limits: Arc::new(limits),
            resources: Arc::new(report),
        }
    }

    /// The single bootstrap point.
    ///
    /// Resolves and parses the settings document exactly once, probes the
    /// machine exactly once, then derives the directory set, the memory budget,
    /// the limits and the override list from them. There is no process cache
    /// behind the returned value: a broken or unreadable document is a hard
    /// error, and a configuration whose worst case cannot fit its budget is
    /// also a hard error — the process refuses to start instead of running
    /// until it is killed for memory (user story 25).
    pub fn bootstrap(conf: Option<&Path>) -> Result<Config, String> {
        let config = Self::inspect(conf)?;
        match &config.resources.refusal {
            Some(reason) => Err(reason.clone()),
            None => Ok(config),
        }
    }

    /// [`Config::bootstrap`] without the refusal check.
    ///
    /// The CLI uses this: a document the daemon refuses is exactly the moment an
    /// operator needs `svc status --resources` and `svc config tune` to explain
    /// why, so the report has to be readable even when the configuration is not
    /// runnable.
    pub fn inspect(conf: Option<&Path>) -> Result<Config, String> {
        let (mut settings, settings_path) = settings::load(conf)?;
        // The two loop cadences still take their historical `SVCD_*` env
        // override. Applying it here keeps "env is read at bootstrap" true for
        // every key, and keeps the override out of the modules below.
        settings::apply_loop_env_overrides(&mut settings);
        Ok(Self::assembled(&settings, settings_path, &resources::Host))
    }

    /// [`Config::bootstrap`] with the machine injected, so a test can state the
    /// host the way it states the document. It keeps the refusal gate: this is
    /// the entry that decides whether a process may run.
    #[cfg(test)]
    pub fn bootstrap_with(
        settings: &Settings,
        settings_path: Option<PathBuf>,
        machine: &dyn Machine,
    ) -> Result<Config, String> {
        let config = Self::assembled(settings, settings_path, machine);
        match &config.resources.refusal {
            Some(reason) => Err(reason.clone()),
            None => Ok(config),
        }
    }

    /// Probe the injected machine and derive, refusing nothing.
    fn assembled(
        settings: &Settings,
        settings_path: Option<PathBuf>,
        machine: &dyn Machine,
    ) -> Config {
        let paths = Paths::resolve(settings);
        let probe_paths = ProbePaths::host(&paths.data_dir, &paths.temp_dir);
        let profile = resources::probe(machine, &probe_paths);
        Self::with_resources(settings.clone(), settings_path, paths, &profile)
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

    /// [`Config::inspect`] with the same reporting shape. A refusal is reported
    /// as a warning and does not stop the command.
    pub fn inspect_or_report(label: &str) -> Option<Config> {
        match Self::inspect(None) {
            Ok(config) => {
                if let Some(reason) = &config.resources.refusal {
                    eprintln!("{}: warning: {}", label, reason);
                }
                Some(config)
            }
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
    use crate::platform::resources::{BudgetSource, Machine, Workload, WorkloadSource};
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

    /// A machine stated by the test: one number for free memory.
    struct Fixed(Option<u64>);

    impl Machine for Fixed {
        fn read(&self, _path: &Path) -> Option<String> {
            None
        }

        fn physical_memory(&self) -> Option<(u64, u64)> {
            self.0.map(|available| (available, available * 2))
        }

        fn cpu_count(&self) -> Option<usize> {
            Some(4)
        }

        fn free_disk(&self, _path: &Path) -> Option<u64> {
            Some(10 * 1024 * 1024 * 1024)
        }
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
    fn the_test_seam_derives_from_the_build_ceiling_and_not_from_this_machine() {
        let config = Config::with_paths(
            Settings::default(),
            None,
            Paths::under(Path::new("/tmp/svcd-config-neutral")),
        );

        assert_eq!(config.resources.budget.quantized, limits::BUDGET_CAP);
        assert_eq!(config.resources.budget.source, BudgetSource::Auto);
        assert_eq!(config.resources.workload_source, WorkloadSource::Derived);
        assert!(config.resources.notes.is_empty());
        assert_eq!(config.resources.refusal, None);
    }

    #[test]
    fn bootstrap_with_states_the_machine_and_reports_it() {
        let settings = Settings::default();
        let config = Config::bootstrap_with(
            &settings,
            Some(PathBuf::from("/etc/svcd/svcd.conf")),
            &Fixed(Some(16 * 1024 * 1024 * 1024)),
        )
        .expect("bootstrap");

        // 25% of 16 GiB is 4 GiB, which the build ceiling still caps.
        assert_eq!(config.resources.budget.raw, limits::BUDGET_CAP);
        assert_eq!(config.resources.profile.cpus, Some(4));
        assert_eq!(
            config.resources.profile.install_free_disk,
            Some(10 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn a_small_machine_derives_a_smaller_shape() {
        let settings = Settings::default();
        let config = Config::bootstrap_with(&settings, None, &Fixed(Some(1024 * 1024 * 1024)))
            .expect("bootstrap");

        // 25% of 1 GiB drives the budget, and 256 MiB is a tiny shape.
        assert_eq!(config.resources.budget.raw, 256 * 1024 * 1024);
        assert_eq!(config.resources.workload, Workload::Tiny);
        assert_eq!(config.resources.workload_source, WorkloadSource::Derived);
    }

    #[test]
    fn an_explicit_workload_overrides_the_derived_one() {
        let settings = Settings {
            workload: Some(Workload::Streaming),
            ..Settings::default()
        };
        let config = Config::bootstrap_with(&settings, None, &Fixed(Some(256 * 1024 * 1024)))
            .expect("bootstrap");

        assert_eq!(config.resources.workload, Workload::Streaming);
        assert_eq!(config.resources.workload_source, WorkloadSource::Configured);
    }

    #[test]
    fn a_config_that_cannot_fit_its_budget_refuses_to_start() {
        // A 64 MiB budget with a 8 MiB stack on ten threads cannot be planned:
        // the stack term alone is larger than the whole budget.
        let settings = Settings {
            mem_budget_bytes: Some(64 * 1024 * 1024),
            request_stack_bytes: Some(8 * 1024 * 1024),
            blocking_threads: Some(8),
            ..Settings::default()
        };

        let error = Config::bootstrap_with(&settings, None, &Fixed(None)).expect_err("must refuse");
        assert!(error.contains("does not fit"), "{error}");
        assert!(error.contains("memory budget"), "{error}");
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
