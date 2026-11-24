//! The import half of a package upload: classify the first tar entry as a
//! package, a .spkb backup or a bare config, then answer in the same shape.

use std::sync::Arc;

use crate::face::support::{self, CallBudget};
use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage::ops::transfer;
use crate::platform::config::Config;
use crate::platform::paths::Paths;

use super::backup::BackupImport;
use super::{import_outcome_json, import_response, install_staged};

pub(super) struct ImportParams {
    pub(super) config_id: Option<String>,
    pub(super) no_config: bool,
    pub(super) force: bool,
}

#[allow(clippy::large_enum_variant)] // Per-upload staging state; the small variants are the hot path.
enum ImportStage {
    /// Fewer than 512 bytes have arrived; the first tar header decides between
    /// a package, a backup and a bare config.
    Classify(Vec<u8>),
    Package {
        installer: package::StagingInstaller,
    },
    Backup(BackupImport),
    Raw(Vec<u8>),
}

pub(super) struct ImportFlow {
    paths: Arc<Paths>,
    limits: Arc<crate::platform::limits::Limits>,
    allow_insecure: bool,
    params: ImportParams,
    stage: ImportStage,
}

impl ImportFlow {
    pub(super) fn new(config: &Config, params: ImportParams) -> ImportFlow {
        ImportFlow {
            paths: config.paths.clone(),
            limits: config.limits.clone(),
            allow_insecure: config.settings.allow_insecure_install,
            params,
            stage: ImportStage::Classify(Vec::new()),
        }
    }

    pub(super) fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        match &mut self.stage {
            ImportStage::Classify(prefix) => {
                prefix.extend_from_slice(data);
                if prefix.len() >= 512 {
                    let data = std::mem::take(prefix);
                    self.classify(data)?;
                }
                Ok(())
            }
            ImportStage::Package { installer } => installer.feed(data),
            ImportStage::Backup(import) => import.feed(data),
            ImportStage::Raw(config) => {
                config.extend_from_slice(data);
                if config.len() as u64 > self.limits.max_body {
                    return Err(Problem::TooLarge.error("import is too large"));
                }
                Ok(())
            }
        }
    }

    fn classify(&mut self, data: Vec<u8>) -> Result<(), ProblemError> {
        let first = package::tar_header_path(&data).ok().flatten();
        match first.as_deref() {
            Some("manifest.json") => {
                // An import asks for no particular root: the size rule picks
                // one, unless the plugin is already installed somewhere.
                let mut installer = package::StagingInstaller::new(
                    self.paths.as_ref().clone(),
                    self.allow_insecure,
                    self.limits.max_package_bytes,
                    None,
                );
                installer.feed(&data)?;
                self.stage = ImportStage::Package { installer };
            }
            Some("backup.json") => {
                let mut import = BackupImport::new(
                    &self.params,
                    self.paths.clone(),
                    self.limits.clone(),
                    self.allow_insecure,
                );
                import.feed(&data)?;
                self.stage = ImportStage::Backup(import);
            }
            _ => {
                if data.len() as u64 > self.limits.max_body {
                    return Err(Problem::TooLarge.error("import is too large"));
                }
                self.stage = ImportStage::Raw(data);
            }
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, config: &Config, budget: &CallBudget) -> Vec<u8> {
        loop {
            let stage = std::mem::replace(&mut self.stage, ImportStage::Raw(Vec::new()));
            match stage {
                ImportStage::Classify(prefix) => {
                    if let Err(error) = self.classify(prefix) {
                        return support::problem_response(&error);
                    }
                    continue;
                }
                ImportStage::Package { mut installer } => {
                    let staged = match installer.finish() {
                        Ok(staged) => staged,
                        Err(error) => return support::problem_response(&error),
                    };
                    let root = staged.root.clone();
                    return match install_staged(
                        config,
                        &root,
                        staged.manifest.id(),
                        &staged.manifest.version,
                        "replace",
                        false,
                        budget,
                    ) {
                        Ok(status) => import_response(status, None, false, Vec::new()),
                        Err(response) => response,
                    };
                }
                ImportStage::Backup(mut import) => {
                    return match import.finish(config, budget) {
                        Ok(response) => response,
                        Err(error) => support::problem_response(&error),
                    }
                }
                ImportStage::Raw(data) => return raw_config_response(&data, &self.params, config),
            }
        }
    }
}

fn raw_config_response(data: &[u8], params: &ImportParams, config: &Config) -> Vec<u8> {
    match transfer::import(
        &config.paths,
        config.settings.allow_insecure_install,
        data,
        params.config_id.as_deref(),
        params.no_config,
        params.force,
        false,
    ) {
        Ok(outcome) => import_outcome_json(outcome),
        Err(transfer::Error::Usage(message)) => {
            support::problem_response(&Problem::Invalid.error(message))
        }
        Err(transfer::Error::Failed(error)) => support::problem_response(&error),
    }
}
