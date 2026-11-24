//! The install half of a package upload: feed the staged tar into a
//! StagingInstaller and hand the finished tree to the daemon.

use std::sync::Arc;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::face::support::{self, CallBudget};
use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::platform::config::Config;
use crate::platform::paths::Paths;
use crate::server::http::model as http;

use super::install_staged;

pub(super) struct InstallFlow {
    paths: Arc<Paths>,
    /// The root the operator asked for (`?root=`), if any. `None` means the
    /// size rule decides (ADR-0030).
    requested_root: Option<std::path::PathBuf>,
    limits: Arc<crate::platform::limits::Limits>,
    allow_insecure: bool,
    strategy: String,
    enable: bool,
    allow_hooks: bool,
    installer: Option<package::StagingInstaller>,
    /// sha256 of the package body. The store's `package_blob` imports the same
    /// bytes into `<temp>` as `sha256:<hex>`, so the install can release that
    /// entry when the attempt ends (#288).
    digest: Sha256,
}

impl InstallFlow {
    pub(super) fn new(
        config: &Config,
        strategy: String,
        enable: bool,
        allow_hooks: bool,
        requested_root: Option<std::path::PathBuf>,
    ) -> InstallFlow {
        InstallFlow {
            paths: config.paths.clone(),
            requested_root,
            limits: config.limits.clone(),
            allow_insecure: config.settings.allow_insecure_install,
            strategy,
            enable,
            allow_hooks,
            installer: None,
            digest: Sha256::new(),
        }
    }

    pub(super) fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        self.digest.update(data);
        self.installer()?.feed(data)
    }

    fn installer(&mut self) -> Result<&mut package::StagingInstaller, ProblemError> {
        if self.installer.is_none() {
            self.installer = Some(package::StagingInstaller::new(
                self.paths.as_ref().clone(),
                self.allow_insecure,
                self.limits.max_package_bytes,
                self.requested_root.clone(),
            ));
        }
        Ok(self
            .installer
            .as_mut()
            .expect("staging installer initialized"))
    }

    pub(super) fn finish(&mut self, config: &Config, budget: &CallBudget) -> Vec<u8> {
        let Some(mut installer) = self.installer.take() else {
            return support::problem_response(&Problem::Invalid.error("manifest.json is missing"));
        };
        let staged = match installer.finish() {
            Ok(staged) => staged,
            Err(error) => return support::problem_response(&error),
        };
        let root = staged.root.clone();
        let id = staged.manifest.id().to_string();
        let version = staged.manifest.version.clone();
        match install_staged(
            config,
            &root,
            &id,
            &version,
            &self.strategy,
            self.allow_hooks,
            budget,
        ) {
            Ok(status) => {
                if !self.enable {
                    let _ = support::rpc(
                        config.paths.control_dir(),
                        "svc.disable",
                        json!({ "id": id }),
                        &budget.follow_up(config.limits.control_timeout),
                    );
                }
                let summary = status.get("summary").cloned().unwrap_or(Value::Null);
                let body = serde_json::to_vec(&json!({
                    "id": summary.get("id").cloned().unwrap_or(Value::Null),
                    "version": summary.get("version").cloned().unwrap_or(Value::Null),
                    "state": summary.get("state").cloned().unwrap_or(Value::Null),
                }))
                .unwrap_or_default();
                http::response(200, "application/json", &body)
            }
            Err(response) => response,
        }
    }
}

/// The upload is a one-shot transfer, so the blob the store imported for it is
/// garbage whichever way the attempt ended: installed, refused, or the client
/// vanished mid-body. `Drop` covers all three, so no finish path has to
/// remember (#288).
impl Drop for InstallFlow {
    fn drop(&mut self) {
        let digest = self.digest.clone().finalize();
        let removed = crate::platform::storage::release(&self.paths, &digest);
        if removed > 0 {
            debug!(
                "install released the {} temp entr{} it consumed",
                removed,
                if removed == 1 { "y" } else { "ies" }
            );
        }
    }
}
