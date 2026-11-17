//! Import semantics shared by `plugin import` and `POST /api/v1/import`.
//!
//! Three inputs are recognised, in this order: a `.spkb` backup (first entry
//! `backup.json`), a `.spk` package (first entry `manifest.json`), and a bare
//! config. Keeping the classification, validation and install call here means
//! the CLI and the HTTP surface cannot drift apart.
//!
//! A failure carries the [`Problem`] raised by the layer that rejected the
//! input (#151); the HTTP face projects the category to a status and never
//! parses the detail text.

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use crate::channel;
use crate::kernel::failure::{Problem, ProblemError};
use crate::package;
use crate::paths;
use crate::plugin;
use crate::settings;

const MAX_IMPORT: u64 = 512 * 1024 * 1024;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// What an import delivered.
#[derive(Debug)]
pub struct Outcome {
    /// The installed package, when the input was `.spk` or `.spkb`. This is the
    /// daemon's `plugin.install` result, kept as JSON: the manage layer does
    /// not own the CLI/HTTP projection of a status (§7).
    pub status: Option<serde_json::Value>,
    /// The plugin whose config was replaced, when a config was applied.
    pub config: Option<String>,
    /// Non-fatal notes, such as importing over a different installed version.
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum Error {
    /// The request is missing an argument; the CLI maps this to exit 64.
    Usage(String),
    /// The input or the daemon rejected the import.
    Failed(ProblemError),
}

pub fn import(
    data: &[u8],
    config_id: Option<&str>,
    no_config: bool,
    force: bool,
    require_root: bool,
) -> Result<Outcome, Error> {
    if data.len() as u64 > MAX_IMPORT {
        return Err(Error::Failed(Problem::TooLarge.error(format!(
            "import exceeds {} MiB",
            MAX_IMPORT / (1024 * 1024)
        ))));
    }
    let entries = package::read_tar(data).ok();
    let first = entries.as_ref().and_then(|entries| entries.first());
    match first.map(|entry| entry.path.as_str()) {
        Some("backup.json") => import_backup(
            entries.as_ref().expect("entries"),
            no_config,
            force,
            require_root,
        ),
        Some("manifest.json") => {
            let status = install_bytes(data, "spk").map_err(Error::Failed)?;
            Ok(Outcome {
                status: Some(status),
                config: None,
                warnings: Vec::new(),
            })
        }
        _ => {
            let Some(id) = config_id else {
                return Err(Error::Usage("raw config requires --config <id>".into()));
            };
            apply_config(id, data, require_root)?;
            Ok(Outcome {
                status: None,
                config: Some(id.to_string()),
                warnings: Vec::new(),
            })
        }
    }
}

fn import_backup(
    entries: &[package::TarEntry],
    no_config: bool,
    force: bool,
    require_root: bool,
) -> Result<Outcome, Error> {
    let backup = entries
        .iter()
        .find(|entry| entry.path == "backup.json")
        .ok_or_else(|| Error::Failed(Problem::Invalid.error("backup.json is missing")))?;
    let backup: plugin::Backup = serde_json::from_slice(&backup.data).map_err(|error| {
        Error::Failed(Problem::Invalid.error(format!("invalid backup.json: {}", error)))
    })?;
    if backup.schema != 1 {
        return Err(Error::Failed(Problem::Invalid.error(format!(
            "unsupported backup schema {}",
            backup.schema
        ))));
    }
    let spk = entries
        .iter()
        .find(|entry| entry.path == "plugin.spk")
        .ok_or_else(|| Error::Failed(Problem::Invalid.error("plugin.spk is missing")))?;
    let (package, _) =
        package::verify_bytes_with_fallback(&spk.data, settings::allow_insecure_install())
            .map_err(Error::Failed)?;
    if package.manifest.name != backup.id {
        return Err(Error::Failed(Problem::Invalid.error(format!(
            "backup id {} does not match package id {}",
            backup.id, package.manifest.name
        ))));
    }
    if package.manifest.version != backup.version {
        return Err(Error::Failed(Problem::Invalid.error(format!(
            "backup version {} does not match package version {}",
            backup.version, package.manifest.version
        ))));
    }
    if let Some(expected) = &backup.config_sha256 {
        let config = entries
            .iter()
            .find(|entry| entry.path == "config")
            .ok_or_else(|| {
                Error::Failed(Problem::Invalid.error(
                    "backup declares a config hash but config is missing",
                ))
            })?;
        if &package::hex_sha256(&config.data) != expected {
            return Err(Error::Failed(Problem::Invalid.error(
                "backup config hash does not match",
            )));
        }
    }

    let mut warnings = Vec::new();
    if let Some(current) = plugin::current_version(&backup.id) {
        if current != backup.version {
            warnings.push(format!(
                "installed {} is {}, importing {}",
                backup.id, current, backup.version
            ));
        } else if !force {
            return Err(Error::Failed(Problem::Conflict.error(format!(
                "{} {} is already installed; use --force",
                backup.id, backup.version
            ))));
        }
    }

    let config = entries.iter().find(|entry| entry.path == "config");
    let with_config = !no_config && config.is_some();
    if with_config && require_root {
        plugin::root_required().map_err(Error::Failed)?;
    }
    let status = install_bytes(&spk.data, "spkb").map_err(Error::Failed)?;
    let config = if let Some(config) = config.filter(|_| with_config) {
        apply_config(&backup.id, &config.data, require_root)?;
        Some(backup.id.clone())
    } else {
        None
    };
    Ok(Outcome {
        status: Some(status),
        config,
        warnings,
    })
}

pub(crate) fn apply_config(id: &str, data: &[u8], require_root: bool) -> Result<(), Error> {
    if require_root {
        plugin::root_required().map_err(Error::Failed)?;
    }
    if !package::valid_name(id) || plugin::current_version(id).is_none() {
        return Err(Error::Failed(Problem::UnknownPlugin.error(format!(
            "unknown plugin: {}",
            id
        ))));
    }
    let _lock = plugin::lock_config(id).map_err(Error::Failed)?;
    plugin::replace_config(id, data, true).map_err(Error::Failed)
}

fn install_bytes(bytes: &[u8], source: &str) -> Result<serde_json::Value, ProblemError> {
    let directory = paths::config_dir().join("tmp");
    fs::create_dir_all(&directory).map_err(|error| {
        Problem::Internal.error(format!("create {}: {}", directory.display(), error))
    })?;
    let path = directory.join(format!(
        "import-{}-{}-{}.spk",
        source,
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = fs::write(&path, bytes) {
        return Err(Problem::Internal.error(format!(
            "write {}: {}",
            path.display(),
            error
        )));
    }
    let response = channel::request(
        &paths::config_dir(),
        "plugin.install",
        json!({ "path": path.to_string_lossy(), "strategy": "replace" }),
    );
    let _ = fs::remove_file(&path);
    let response = response.map_err(|error| {
        Problem::Internal.error(format!("svcd unreachable: {}", error))
    })?;
    if let Some(error) = response.error {
        // The daemon already named the category on the wire (S5b); the detail
        // is only for the human reading the error.
        let problem = Problem::from_error_data(error.data.as_ref()).unwrap_or(Problem::Internal);
        return Err(problem.error(error.message));
    }
    let result = response
        .result
        .ok_or_else(|| Problem::Internal.error("empty install response"))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_config_without_an_id_is_a_usage_error() {
        match import(b"plain text", None, false, false, true) {
            Err(Error::Usage(message)) => assert!(message.contains("--config"), "{message}"),
            other => panic!("expected a usage error, got {:?}", other),
        }
    }

    #[test]
    fn a_missing_backup_entry_is_invalid() {
        let entries = vec![package::TarEntry {
            path: "backup.json".to_string(),
            mode: 0o644,
            data: b"{\"schema\":1,\"id\":\"demo\",\"version\":\"1.0.0\"}".to_vec(),
        }];
        match import_backup(&entries, false, false, false) {
            Err(Error::Failed(error)) => {
                assert_eq!(error.problem, Problem::Invalid);
                assert!(error.detail().contains("plugin.spk"), "{}", error);
            }
            other => panic!("expected a failed import, got {:?}", other),
        }
    }
}
