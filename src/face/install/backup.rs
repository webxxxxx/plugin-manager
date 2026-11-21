//! A .spkb backup arriving inside an import upload: backup.json, the nested
//! .spk and an optional config entry, parsed as they stream in.

use std::path::PathBuf;
use std::sync::Arc;

use crate::face::support::CallBudget;
use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::ops::transfer;
use crate::platform::config::Config;
use crate::platform::paths::Paths;

use super::import_flow::ImportParams;
use super::{import_response, install_staged};

/// A .spkb backup: backup.json, the nested .spk, and an optional config entry.
/// The outer tar and the inner package are both parsed as they arrive; the
/// inner package reuses package::StagingInstaller.
///
/// transfer::import_backup is the buffered twin used by the CLI and the legacy
/// whole-body import. The validation and force/warning rules live in
/// manage::check::backup; both paths call them instead of keeping a copy. This
/// type keeps only the streaming tar state machine and the staging cleanup.
pub(super) struct BackupImport {
    tar: package::TarFeed,
    sink: BackupSink,
}

impl BackupImport {
    pub(super) fn new(
        params: &ImportParams,
        staging: PathBuf,
        paths: Arc<Paths>,
        limits: Arc<crate::platform::limits::Limits>,
        allow_insecure: bool,
    ) -> BackupImport {
        let max = limits.max_package_bytes;
        BackupImport {
            tar: package::TarFeed::new(max),
            sink: BackupSink::new(params, staging, paths, limits, allow_insecure),
        }
    }

    pub(super) fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        self.tar.feed(data, &mut self.sink)
    }

    pub(super) fn finish(
        &mut self,
        config: &Config,
        budget: &CallBudget,
    ) -> Result<Vec<u8>, ProblemError> {
        self.tar.finish()?;
        self.sink.finish(config, budget)
    }
}

struct BackupSink {
    staging: PathBuf,
    paths: Arc<Paths>,
    limits: Arc<crate::platform::limits::Limits>,
    allow_insecure: bool,
    no_config: bool,
    force: bool,
    backup: Option<plugin::Backup>,
    installer: Option<package::StagingInstaller>,
    staged: Option<package::StagedPackage>,
    config_data: Option<Vec<u8>>,
    stage: BackupStage,
    current: Option<BackupCurrent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupStage {
    ExpectBackup,
    ExpectSpk,
    ExpectConfig,
    AfterConfig,
}

enum BackupCurrent {
    Backup(Vec<u8>),
    Spk,
    Config(Vec<u8>),
}

impl BackupSink {
    fn new(
        params: &ImportParams,
        staging: PathBuf,
        paths: Arc<Paths>,
        limits: Arc<crate::platform::limits::Limits>,
        allow_insecure: bool,
    ) -> BackupSink {
        BackupSink {
            staging,
            paths,
            limits,
            allow_insecure,
            no_config: params.no_config,
            force: params.force,
            backup: None,
            installer: None,
            staged: None,
            config_data: None,
            stage: BackupStage::ExpectBackup,
            current: None,
        }
    }
}

impl package::TarSink for BackupSink {
    fn entry_start(&mut self, entry: &package::StreamEntry) -> Result<(), ProblemError> {
        match self.stage {
            BackupStage::ExpectBackup => {
                if entry.path != "backup.json" {
                    return Err(Problem::Invalid.error("backup.json must be the first entry"));
                }
                if entry.size > 64 * 1024 {
                    return Err(Problem::TooLarge.error("backup.json is too large"));
                }
                self.current = Some(BackupCurrent::Backup(Vec::with_capacity(
                    entry.size as usize,
                )));
            }
            BackupStage::ExpectSpk => {
                if entry.path != "plugin.spk" {
                    return Err(Problem::Invalid.error("plugin.spk must be the second entry"));
                }
                if entry.kind != b'0' {
                    return Err(
                        Problem::Invalid.error(format!("{} is not a regular file", entry.path))
                    );
                }
                self.installer = Some(package::StagingInstaller::new(
                    self.staging.clone(),
                    self.allow_insecure,
                    self.limits.max_package_bytes,
                    &self.paths.trust_dir,
                ));
                self.current = Some(BackupCurrent::Spk);
            }
            BackupStage::ExpectConfig => {
                if entry.path != "config" {
                    return Err(Problem::Invalid.error(format!("unexpected entry: {}", entry.path)));
                }
                if entry.size > 64 * 1024 {
                    return Err(Problem::TooLarge.error("config is too large"));
                }
                self.current = Some(BackupCurrent::Config(Vec::with_capacity(
                    entry.size as usize,
                )));
                self.stage = BackupStage::AfterConfig;
            }
            BackupStage::AfterConfig => {
                return Err(Problem::Invalid.error("unexpected entries after config"));
            }
        }
        Ok(())
    }

    fn entry_data(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        match self.current.as_mut() {
            Some(BackupCurrent::Backup(buffer)) | Some(BackupCurrent::Config(buffer)) => {
                buffer.extend_from_slice(data);
                Ok(())
            }
            Some(BackupCurrent::Spk) => self
                .installer
                .as_mut()
                .expect("package installer initialized")
                .feed(data),
            None => Err(Problem::Invalid.error("tar entry data without a header")),
        }
    }

    fn entry_end(&mut self) -> Result<(), ProblemError> {
        let Some(current) = self.current.take() else {
            return Err(Problem::Invalid.error("tar entry ended without a header"));
        };
        match current {
            BackupCurrent::Backup(bytes) => {
                self.backup = Some(plugin::parse_backup(&bytes)?);
                self.stage = BackupStage::ExpectSpk;
                Ok(())
            }
            BackupCurrent::Spk => {
                let mut installer = self
                    .installer
                    .take()
                    .ok_or_else(|| Problem::Invalid.error("plugin.spk is missing"))?;
                let staged = installer.finish()?;
                // A later rule check may fail; keep the staged tree in
                // self.staged so the sink's Drop removes it.
                self.staged = Some(staged);
                if self.backup.is_none() {
                    return Err(Problem::Invalid.error("backup.json is missing"));
                }
                self.stage = BackupStage::ExpectConfig;
                Ok(())
            }
            BackupCurrent::Config(bytes) => {
                self.config_data = Some(bytes);
                self.stage = BackupStage::AfterConfig;
                Ok(())
            }
        }
    }
}

impl BackupSink {
    fn finish(&mut self, config: &Config, budget: &CallBudget) -> Result<Vec<u8>, ProblemError> {
        let backup = self
            .backup
            .take()
            .ok_or_else(|| Problem::Invalid.error("backup.json is missing"))?;
        // The rules may fail; keep the staged tree in self.staged so Drop
        // removes the directory.
        let staged = self
            .staged
            .as_ref()
            .ok_or_else(|| Problem::Invalid.error("plugin.spk is missing"))?;
        let current = plugin::current_version(&self.paths, &backup.id);
        let plan = plugin::BackupRules {
            backup: &backup,
            package_id: &staged.manifest.name,
            package_version: &staged.manifest.version,
            config: self.config_data.as_deref(),
            current_version: current.as_deref(),
            force: self.force,
        }
        .plan()?;
        let staged = self.staged.take().expect("staged package");
        let applied = !self.no_config && self.config_data.is_some();
        let status = match install_staged(
            config,
            &staged.root,
            &staged.manifest.name,
            &staged.manifest.version,
            "replace",
            false,
            budget,
        ) {
            Ok(status) => status,
            Err(response) => return Ok(response),
        };
        if applied {
            transfer::apply_config(
                &config.paths,
                &backup.id,
                self.config_data.as_ref().expect("config data"),
                false,
            )
            .map_err(|error| match error {
                transfer::Error::Usage(message) => Problem::Invalid.error(message),
                transfer::Error::Failed(error) => error,
            })?;
        }
        Ok(import_response(
            status,
            Some(backup.id),
            applied,
            plan.warnings,
        ))
    }
}

impl Drop for BackupSink {
    fn drop(&mut self) {
        if let Some(staged) = self.staged.take() {
            let _ = std::fs::remove_dir_all(&staged.root);
        }
    }
}
