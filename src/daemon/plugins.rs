use std::path::Path;

use crate::face::control;
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::projection::Status;
use crate::kernel::registry::Kind;
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::Strategy;
use crate::platform::clock::Clock;
use crate::platform::log;

use super::Daemon;

impl Daemon {
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
        let id = package.manifest.name.clone();
        if let Some(existing) = self.host.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::CoreService,
                    format!("id is a core service: {}", id),
                ));
            }
        }

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
                log::svcd(&format!(
                    "svcd: plugin {}: keep-old ran out of space; falling back to replace",
                    id
                ));
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
        self.host
            .install(service, still_running)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        self.bump_catalog();
        log::svcd(&format!(
            "svcd: plugin {} {} installed ({}){}",
            id,
            package.manifest.version,
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
        ));
        self.host.status_for_id(&id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
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
        let id = staged.manifest.name.clone();
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
        if let Some(existing) = self.host.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::CoreService,
                    format!("id is a core service: {}", id),
                ));
            }
        }
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
        self.host
            .install(service, still_running)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        self.bump_catalog();
        log::svcd(&format!(
            "svcd: plugin {} {} installed ({}){}",
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
        ));
        self.host.status_for_id(&id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
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
        log::svcd(&format!("svcd: plugin {} uninstalled", id));
        Ok(())
    }
}
