use std::path::Path;

use crate::face::control;
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::projection::Status;
use crate::kernel::registry::{Kind, Service};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::Strategy;
use crate::platform::clock::Clock;

use super::Daemon;

/// The one install log line. Only two facts vary between the direct and staged
/// paths: whether a running process keeps the old files until its next start,
/// and whether the signature check was skipped. Both paths spell them here so
/// the text cannot drift apart (#245).
fn install_log_line(
    id: &str,
    version: &str,
    still_running: bool,
    signature_verified: bool,
) -> String {
    format!(
        "plugin {} {} installed ({}){}",
        id,
        version,
        if still_running {
            "running; next start uses"
        } else {
            "now"
        },
        if signature_verified {
            ""
        } else {
            "; signature was NOT verified (allow_insecure_install)"
        }
    )
}

impl Daemon {
    /// The same core-service guard on both install paths: a package may replace
    /// a plugin, never a service the daemon itself owns. It runs after the id is
    /// known and before anything is stopped or written.
    fn reject_core_service(
        &self,
        id: &str,
        display_name: Option<&str>,
    ) -> Result<(), ProblemError> {
        match self.host.registry.core_conflict(id, display_name) {
            Some(message) => Err(ProblemError::new(Problem::Conflict, message)),
            None => Ok(()),
        }
    }

    /// The tail both install paths share once they hold the final `Service`:
    /// commit it to the supervisor, invalidate the tool catalog, write the one
    /// install line, then project the status the caller returns. Keeping these
    /// four steps here is what makes the direct and staged paths end the same
    /// way (#245).
    fn finish_install(
        &mut self,
        id: &str,
        version: &str,
        signature_verified: bool,
        service: Service,
        still_running: bool,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        self.host.install(service, still_running)?;
        self.bump_catalog();
        info!(
            "{}",
            install_log_line(id, version, still_running, signature_verified,)
        );
        self.host.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    pub(super) fn install_plugin(
        &mut self,
        install: control::Install,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        let path = install.path.as_str();
        let strategy = install.strategy;
        let allow_hooks = install.allow_hooks;

        // Everything that can reject the package runs before anything is
        // stopped or written: a bad package must leave the installed version
        // untouched. `plugin::verify` is the same preflight `plugin verify`
        // exposes, so the two answers cannot drift apart.
        let verified = plugin::verify(
            &self.config.paths,
            Path::new(path),
            self.config.settings.allow_insecure_install,
            allow_hooks,
        )?;
        let plugin::Verified {
            package,
            signature_verified,
            interpreter: host_interpreter,
        } = verified;
        let id = package.manifest.id().to_string();
        self.reject_core_service(&id, Some(&package.manifest.name))?;

        if self.host.is_running(&id) && strategy == Strategy::Replace {
            self.host.stop_service(&id, clock)?;
        }
        let still_running = self.host.is_running(&id);
        let mut poll = self.host.hook_pump();
        let service = match plugin::install_package(
            &self.config.paths,
            &self.config.limits,
            &package,
            strategy,
            allow_hooks,
            host_interpreter.clone(),
            &mut poll,
        ) {
            Ok(service) => service,
            Err(plugin::InstallFailure::OutOfSpace(error)) if strategy == Strategy::KeepOld => {
                if still_running {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!(
                            "not enough space for keep-old; stop the plugin and retry with replace: {}",
                            error
                        ),
                    ));
                }
                warn!(
                    "plugin {}: keep-old ran out of space; falling back to replace",
                    id
                );
                plugin::install_package(
                    &self.config.paths,
                    &self.config.limits,
                    &package,
                    Strategy::Replace,
                    allow_hooks,
                    host_interpreter,
                    &mut poll,
                )
                .map_err(plugin::InstallFailure::into_problem)?
            }
            Err(failure) => return Err(failure.into_problem()),
        };
        self.finish_install(
            &id,
            &package.manifest.version,
            signature_verified,
            service,
            still_running,
            clock,
        )
    }

    pub(super) fn install_staged(
        &mut self,
        install: control::Install,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        let path = install.path.as_str();
        let strategy = install.strategy;
        let allow_hooks = install.allow_hooks;

        let staging = Path::new(path).canonicalize().map_err(|error| {
            ProblemError::new(
                Problem::Internal,
                format!("staging path {}: {}", path, error),
            )
        })?;
        let plugins = self
            .config
            .paths
            .plugins_dir
            .canonicalize()
            .map_err(|error| {
                ProblemError::new(Problem::Internal, format!("plugins dir: {}", error))
            })?;
        if !staging.starts_with(&plugins)
            || !staging
                .file_name()
                .map(|name| name.to_string_lossy().starts_with(".staging-"))
                .unwrap_or(false)
        {
            return Err(ProblemError::new(
                Problem::Internal,
                "staging path is outside the plugin staging area",
            ));
        }

        let staged = package::load_staged(
            &staging,
            self.config.settings.allow_insecure_install,
            &self.config.paths.trust_dir,
        )?;
        let id = staged.manifest.id().to_string();
        let version = staged.manifest.version.clone();
        if let Some(requested) = install.expected_id.as_deref() {
            if requested != id {
                return Err(ProblemError::new(
                    Problem::Internal,
                    "staged id does not match the request",
                ));
            }
        }
        if let Some(requested) = install.expected_version.as_deref() {
            if requested != version {
                return Err(ProblemError::new(
                    Problem::Internal,
                    "staged version does not match the request",
                ));
            }
        }
        self.reject_core_service(&id, Some(&staged.manifest.name))?;
        let host_interpreter = plugin::prepare_staged(&staged, allow_hooks)?;
        let signature_verified = staged.signature_verified;

        if self.host.is_running(&id) && strategy == Strategy::Replace {
            self.host.stop_service(&id, clock)?;
        }
        let still_running = self.host.is_running(&id);
        let mut poll = self.host.hook_pump();
        let service = plugin::install_staged(
            &self.config.paths,
            &self.config.limits,
            staged,
            strategy,
            allow_hooks,
            host_interpreter,
            &mut poll,
        )?;
        self.finish_install(
            &id,
            &version,
            signature_verified,
            service,
            still_running,
            clock,
        )
    }

    pub(super) fn uninstall_plugin(
        &mut self,
        id: &str,
        purge: bool,
        clock: &Clock,
    ) -> Result<(), ProblemError> {
        let service = self.host.registry.get(id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })?;
        if service.kind != Kind::Plugin {
            return Err(ProblemError::new(
                Problem::NotAPlugin,
                format!("not a plugin: {}", id),
            ));
        }
        if self.host.is_running(id) {
            self.host.stop_service(id, clock)?;
        }
        let mut poll = self.host.hook_pump();
        plugin::uninstall(
            &self.config.paths,
            &self.config.limits,
            id,
            purge,
            &mut poll,
        )?;
        self.host.forget(id);
        self.bump_catalog();
        if purge {
            plugin::remove_logs(&self.config.paths, id);
            #[cfg(feature = "http")]
            self.bus.forget(id);
        }
        info!("plugin {} uninstalled", id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::mpsc;

    use crate::face::control;
    use crate::kernel::registry::{Registry, State};
    use crate::manage::Strategy;
    use crate::platform::clock::Clock;
    use crate::platform::limits;

    use super::{install_log_line, Daemon};

    fn daemon() -> Daemon {
        let (_sender, receiver) = mpsc::channel();
        Daemon::new(
            Registry::with_builtin(&limits::test_default()),
            receiver,
            None,
        )
    }

    fn request(path: &Path) -> control::Install {
        control::Install {
            path: path.to_string_lossy().into_owned(),
            strategy: Strategy::Replace,
            allow_hooks: false,
            expected_id: None,
            expected_version: None,
        }
    }

    #[test]
    fn the_install_log_line_pins_every_running_and_signature_combination() {
        for (still_running, signature_verified, expected) in [
            (
                true,
                true,
                "plugin demo 1.0.0 installed (running; next start uses)",
            ),
            (
                true,
                false,
                "plugin demo 1.0.0 installed (running; next start uses); signature was NOT verified (allow_insecure_install)",
            ),
            (false, true, "plugin demo 1.0.0 installed (now)"),
            (
                false,
                false,
                "plugin demo 1.0.0 installed (now); signature was NOT verified (allow_insecure_install)",
            ),
        ] {
            assert_eq!(
                install_log_line("demo", "1.0.0", still_running, signature_verified),
                expected,
                "still_running={still_running} signature_verified={signature_verified}"
            );
        }
    }

    #[test]
    fn both_install_paths_finish_with_the_same_status() {
        let package =
            crate::manage::test_support::test_signed_package("demo", crate::platform::target());
        let clock = Clock::boot();
        // One daemon owns one config root, so a status difference cannot come
        // from two different plugin/config paths.
        let mut daemon = daemon();
        std::fs::create_dir_all(&daemon.config.paths.plugins_dir).expect("plugins dir");

        // The direct path installs the .spk through verify + install_package.
        let files = crate::manage::test_support::tempdir::TempDir::new();
        let spk = files.path().join("demo.spk");
        std::fs::write(&spk, &package).expect("write package");
        let direct_status = daemon
            .install_plugin(request(&spk), &clock)
            .expect("direct install");

        // The staged path installs the same package from a staging directory
        // inside the plugin tree, the way the HTTP uploader leaves it.
        let staging = daemon.config.paths.plugins_dir.join(".staging-245");
        let mut installer = crate::manage::StagingInstaller::new(
            staging.clone(),
            false,
            8 * 1024 * 1024,
            &daemon.config.paths.trust_dir,
        );
        installer.feed(&package).expect("feed package");
        installer.finish().expect("finish staging");
        let staged_status = daemon
            .install_staged(request(&staging), &clock)
            .expect("staged install");

        assert_eq!(direct_status, staged_status);
        assert_eq!(direct_status.summary.id, "demo");
        assert_eq!(direct_status.summary.version, "1.0.0");
        assert_eq!(direct_status.summary.state, State::Stopped);
        assert!(!direct_status.summary.enabled);
    }
}
