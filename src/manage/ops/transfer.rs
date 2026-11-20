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

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage as package;
use crate::manage as plugin;
use crate::platform::paths::Paths;
use crate::wire::control::channel;
use crate::wire::message::rpc;

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
    paths: &Paths,
    allow_insecure: bool,
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
            paths,
            allow_insecure,
            entries.as_ref().expect("entries"),
            no_config,
            force,
            require_root,
        ),
        Some("manifest.json") => {
            let status = install_bytes(paths, data, "spk").map_err(Error::Failed)?;
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
            apply_config(paths, id, data, require_root)?;
            Ok(Outcome {
                status: None,
                config: Some(id.to_string()),
                warnings: Vec::new(),
            })
        }
    }
}

fn import_backup(
    paths: &Paths,
    allow_insecure: bool,
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
        return Err(Error::Failed(
            Problem::Invalid.error(format!("unsupported backup schema {}", backup.schema)),
        ));
    }
    let spk = entries
        .iter()
        .find(|entry| entry.path == "plugin.spk")
        .ok_or_else(|| Error::Failed(Problem::Invalid.error("plugin.spk is missing")))?;
    let (package, _) =
        package::verify_bytes_with_fallback(&spk.data, allow_insecure, &paths.trust_dir)
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
                Error::Failed(
                    Problem::Invalid.error("backup declares a config hash but config is missing"),
                )
            })?;
        if &package::hex_sha256(&config.data) != expected {
            return Err(Error::Failed(
                Problem::Invalid.error("backup config hash does not match"),
            ));
        }
    }

    let mut warnings = Vec::new();
    if let Some(current) = plugin::current_version(paths, &backup.id) {
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
    let status = install_bytes(paths, &spk.data, "spkb").map_err(Error::Failed)?;
    let config = if let Some(config) = config.filter(|_| with_config) {
        apply_config(paths, &backup.id, &config.data, require_root)?;
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

pub(crate) fn apply_config(
    paths: &Paths,
    id: &str,
    data: &[u8],
    require_root: bool,
) -> Result<(), Error> {
    if require_root {
        plugin::root_required().map_err(Error::Failed)?;
    }
    if !package::valid_name(id) || plugin::current_version(paths, id).is_none() {
        return Err(Error::Failed(
            Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)),
        ));
    }
    let _lock = plugin::lock_config(paths, id).map_err(Error::Failed)?;
    plugin::replace_config(paths, id, data, true).map_err(Error::Failed)
}

fn install_bytes(
    paths: &Paths,
    bytes: &[u8],
    source: &str,
) -> Result<serde_json::Value, ProblemError> {
    let directory = paths.temp_dir.clone();
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
        return Err(Problem::Internal.error(format!("write {}: {}", path.display(), error)));
    }
    let response = channel::request(
        paths.control_dir(),
        "plugin.install",
        json!({ "path": path.to_string_lossy(), "strategy": "replace" }),
    );
    let _ = fs::remove_file(&path);
    let response = response
        .map_err(|error| Problem::Internal.error(format!("svcd unreachable: {}", error)))?;
    refused(response)
}

/// A daemon refusal as a typed failure.
///
/// The control channel names the category in `error.data.kind`; the message is
/// only for people. A message that happens to contain "unknown" (the Linux
/// `x86_64-unknown-linux-gnu` triple) must not become a missing-plugin error
/// (#150 handoff).
fn refused(response: rpc::Response) -> Result<serde_json::Value, ProblemError> {
    if let Some(error) = response.error {
        let problem = Problem::from_error_data(error.data.as_ref()).unwrap_or(Problem::Internal);
        return Err(problem.error(error.message));
    }
    response
        .result
        .ok_or_else(|| Problem::Internal.error("empty install response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_config_without_an_id_is_a_usage_error() {
        match import(
            &Paths::under(std::path::Path::new("/nonexistent-svcd-transfer")),
            false,
            b"plain text",
            None,
            false,
            false,
            true,
        ) {
            Err(Error::Usage(message)) => assert!(message.contains("--config"), "{message}"),
            other => panic!("expected a usage error, got {:?}", other),
        }
    }

    /// The raw `.spk` import reads the daemon's category, not its message: the
    /// host triple in a platform reason contains "unknown", which used to turn
    /// a platform mismatch into 404 (#150 handoff).
    #[test]
    fn a_platform_refusal_keeps_its_wire_category_despite_the_host_triple() {
        let response = rpc::Response {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            result: None,
            error: Some(rpc::Error {
                code: -32006,
                message: "platform mismatch: package other, host x86_64-unknown-linux-gnu"
                    .to_string(),
                data: Some(json!({ "kind": "unsupported_platform" })),
            }),
        };
        let error = refused(response).expect_err("refused");
        assert_eq!(error.problem, Problem::UnsupportedPlatform);
    }

    #[test]
    fn a_refusal_without_a_wire_category_is_internal() {
        let response = rpc::Response {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            result: None,
            error: Some(rpc::Error {
                code: -32603,
                message: "not a plugin: core".to_string(),
                data: None,
            }),
        };
        assert_eq!(
            refused(response).expect_err("refused").problem,
            Problem::Internal
        );
    }

    #[test]
    fn a_missing_backup_entry_is_invalid() {
        let entries = vec![package::TarEntry {
            path: "backup.json".to_string(),
            mode: 0o644,
            data: b"{\"schema\":1,\"id\":\"demo\",\"version\":\"1.0.0\"}".to_vec(),
        }];
        match import_backup(
            &Paths::under(std::path::Path::new("/nonexistent-svcd-transfer")),
            false,
            &entries,
            false,
            false,
            false,
        ) {
            Err(Error::Failed(error)) => {
                assert_eq!(error.problem, Problem::Invalid);
                assert!(error.detail().contains("plugin.spk"), "{}", error);
            }
            other => panic!("expected a failed import, got {:?}", other),
        }
    }
}
