use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::face::control;
use crate::kernel::call::CallId;
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::projection::Status;
use crate::kernel::registry::{Kind, Service};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::Strategy;
use crate::platform::clock::Clock;
use crate::platform::paths::Paths;

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
    // One more argument than clippy likes: the call number joined the two
    // paths' facts. A struct would only rename them, the way the slot's
    // `adopt` keeps its own allow.
    #[allow(clippy::too_many_arguments)]
    fn finish_install(
        &mut self,
        call: CallId,
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
            "call={} {}",
            call,
            install_log_line(id, version, still_running, signature_verified,)
        );
        self.host.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    /// Install one package the caller staged on disk.
    ///
    /// The payload is a one-shot input: the store downloads it into
    /// `<data>/temp`, hands the path over, and only the installed tree has to
    /// survive. A refused package used to leave those bytes for
    /// `temp_retention_secs` (two hours), which is what filled the 34 MB
    /// OpenWrt volume in #286 — so the source is dropped on success *and* on
    /// refusal, as long as it lives inside the temp area the daemon owns.
    pub(super) fn install_plugin(
        &mut self,
        install: control::Install,
        call: CallId,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        let source = PathBuf::from(&install.path);
        let result = self.install_package(install, call, clock);
        cleanup_temp_payload(&self.config.paths, &source);
        result
    }

    fn install_package(
        &mut self,
        install: control::Install,
        call: CallId,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        let path = install.path.as_str();
        let strategy = install.strategy;
        let allow_hooks = install.allow_hooks;
        // An explicit root is only a preference; `install_package` refuses one
        // that is not in `plugins_dir`, and refuses to move an installed
        // plugin to a different root (ADR-0030).
        let requested_root = install.root.clone();

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
        let service = {
            let mut hooks = crate::supervise::ops::hook::Runner::new(
                &self.config.paths,
                &self.config.limits,
                &mut poll,
            );
            match plugin::install_package(
                &self.config.paths,
                &self.config.limits,
                &package,
                strategy,
                allow_hooks,
                host_interpreter.clone(),
                requested_root.as_deref().map(Path::new),
                &mut hooks,
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
                        "call={} plugin {}: keep-old ran out of space; falling back to replace",
                        call, id
                    );
                    plugin::install_package(
                        &self.config.paths,
                        &self.config.limits,
                        &package,
                        Strategy::Replace,
                        allow_hooks,
                        host_interpreter,
                        requested_root.as_deref().map(Path::new),
                        &mut hooks,
                    )
                    .map_err(plugin::InstallFailure::into_problem)?
                }
                Err(failure) => return Err(failure.into_problem()),
            }
        };
        self.finish_install(
            call,
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
        call: CallId,
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
        // A staged tree may be in any plugin root (ADR-0030); what matters is
        // that it is a `.staging-*` directory inside one of them, so the
        // commit's rename stays on one filesystem.
        let mut inside_a_root = false;
        for root in &self.config.paths.plugin_roots {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            if staging.starts_with(&root) {
                inside_a_root = true;
                break;
            }
        }
        if !inside_a_root
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

        // Load from the path as the caller spelled it, not the canonicalized
        // one: the canonical form is only the containment check, and its
        // Windows `\\?\` spelling would be recorded as the plugin's root.
        let staged = package::load_staged(
            Path::new(path),
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
        let service = {
            let mut hooks = crate::supervise::ops::hook::Runner::new(
                &self.config.paths,
                &self.config.limits,
                &mut poll,
            );
            plugin::install_staged(
                &self.config.paths,
                &self.config.limits,
                staged,
                strategy,
                allow_hooks,
                host_interpreter,
                &mut hooks,
            )?
        };
        self.finish_install(
            call,
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
        force: bool,
        clock: &Clock,
    ) -> Result<(), ProblemError> {
        // A plugin on a root that is not available right now is not in the
        // registry either, and the record is the only thing that knows it
        // exists. So a missing id is handed to the manager: it answers 409
        // with the volume path, or "unknown plugin" when there is no record.
        let registered = self.host.registry.get(id).map(|service| service.kind);
        if let Some(kind) = registered {
            if kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::NotAPlugin,
                    format!("not a plugin: {}", id),
                ));
            }
        }
        if self.host.is_running(id) {
            self.host.stop_service(id, clock)?;
        }
        let mut poll = self.host.hook_pump();
        {
            let mut hooks = crate::supervise::ops::hook::Runner::new(
                &self.config.paths,
                &self.config.limits,
                &mut poll,
            );
            plugin::uninstall(&self.config.paths, id, purge, force, &mut hooks)?;
        }
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

pub(super) fn cleanup_temp_payload(paths: &Paths, source: &Path) {
    let Ok(temp) = fs::canonicalize(&paths.temp_dir) else {
        // No temp area yet: nothing of ours can be in it.
        return;
    };
    let Ok(source) = fs::canonicalize(source) else {
        // Already gone (the CLI removes its own import, then calls here).
        return;
    };
    let Some(entry) = temp_entry(&temp, &source) else {
        return;
    };
    let result = match fs::symlink_metadata(&entry) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&entry),
        Ok(_) => fs::remove_file(&entry),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        warn!(
            "temp payload {} not removed after install: {}",
            entry.display(),
            error
        );
    }
}

/// Which temp entry a source path belongs to.
///
/// `files/<id>/...` and `uploads/<id>/...` are bundles: the whole id directory
/// is the unit, or a deleted part would leave the manifest behind. Everything
/// else in temp is a single file (a content-addressed blob) or one
/// `incoming/` result.
fn temp_entry(temp: &Path, source: &Path) -> Option<PathBuf> {
    let relative = source.strip_prefix(temp).ok()?;
    let mut parts = relative.components();
    let Component::Normal(first) = parts.next()? else {
        return None;
    };
    if first == "files" || first == "uploads" {
        let Component::Normal(id) = parts.next()? else {
            // The store directory itself is not a payload.
            return None;
        };
        return Some(temp.join(first).join(id));
    }
    if first == "incoming" {
        let Component::Normal(file) = parts.next()? else {
            return None;
        };
        return Some(temp.join("incoming").join(file));
    }
    Some(temp.join(first))
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

    use super::{install_log_line, CallId, Daemon};

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
            root: None,
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
        std::fs::create_dir_all(daemon.config.paths.default_plugin_root()).expect("plugins dir");

        // The direct path installs the .spk through verify + install_package.
        let files = crate::platform::test_support::TempDir::new();
        let spk = files.path().join("demo.spk");
        std::fs::write(&spk, &package).expect("write package");
        let direct_status = daemon
            .install_plugin(request(&spk), CallId::FIRST, &clock)
            .expect("direct install");

        // The staged path installs the same package from a staging directory
        // inside the plugin tree, the way the HTTP uploader leaves it.
        let mut installer = crate::manage::StagingInstaller::new(
            daemon.config.paths.as_ref().clone(),
            false,
            8 * 1024 * 1024,
            None,
        );
        installer.feed(&package).expect("feed package");
        let staging = installer.finish().expect("finish staging").root;
        let staged_status = daemon
            .install_staged(request(&staging), CallId::FIRST.next(), &clock)
            .expect("staged install");

        assert_eq!(direct_status, staged_status);
        assert_eq!(direct_status.summary.id, "demo");
        assert_eq!(direct_status.summary.version, "1.0.0");
        assert_eq!(direct_status.summary.state, State::Stopped);
        assert!(!direct_status.summary.enabled);
    }
}
