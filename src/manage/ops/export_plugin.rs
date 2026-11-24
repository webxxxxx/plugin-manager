//! 插件导出：`config` 与 `.spkb` 备份。
use std::fs;
use std::path::Path;

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::layout::plugin_dir;
use crate::manage as package;
use crate::manage::model::layout::{active_config, current_version};
use crate::platform::paths::Paths;

pub fn export_config(paths: &Paths, id: &str) -> Result<(String, Vec<u8>), ProblemError> {
    let version = current_version(paths, id)
        .ok_or_else(|| Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)))?;
    let config = active_config(paths, id)
        .ok_or_else(|| Problem::UnknownPlugin.error(format!("plugin {} has no config", id)))?;
    let data = fs::read(&config)
        .map_err(|error| Problem::Internal.error(format!("read config: {}", error)))?;
    Ok((version, data))
}

/// The .spkb layout: backup.json, the nested .spk entry, the active config
/// when there is one, and the tar trailer.
///
/// Shared by the CLI export and the HTTP stream; the nested package is itself
/// a plan, so both callers describe the archive the same way.
pub fn plan_export_spkb(
    id: &str,
    version: &str,
    version_dir: &Path,
    config: Option<&[u8]>,
) -> Result<package::TarPlan, ProblemError> {
    let mut plan = package::TarPlan::new();
    let backup = backup_json(id, version, config);
    plan.push_bytes(package::tar_entry_bytes("backup.json", &backup, 0o644)?);
    let inner = package::plan_installed_spk(version_dir)?;
    let inner_size = inner.size();
    plan.push_bytes(package::tar_header_bytes("plugin.spk", inner_size, 0o644)?);
    plan.append(inner);
    plan.push_bytes(package::tar_padding_bytes(inner_size)?);
    if let Some(config) = config {
        plan.push_bytes(package::tar_entry_bytes("config", config, 0o600)?);
    }
    plan.push_bytes(vec![0u8; package::TAR_TRAILER]);
    Ok(plan)
}

pub fn export_spkb(paths: &Paths, id: &str) -> Result<(String, Vec<u8>), ProblemError> {
    let version = current_version(paths, id)
        .ok_or_else(|| Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)))?;
    let config = active_config(paths, id)
        .map(|path| {
            fs::read(path)
                .map_err(|error| Problem::Internal.error(format!("read config: {}", error)))
        })
        .transpose()?;
    let plan = plan_export_spkb(
        id,
        &version,
        &plugin_dir(paths, id).join(&version),
        config.as_deref(),
    )?;
    let mut bytes = Vec::new();
    plan.write_verified(&mut bytes)?;
    Ok((version, bytes))
}

pub(crate) fn backup_json(id: &str, version: &str, config: Option<&[u8]>) -> Vec<u8> {
    let exported_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "id": id,
        "version": version,
        "exportedAt": exported_at,
        "configSha256": config.map(package::hex_sha256),
    }))
    .unwrap_or_default()
}

pub(crate) fn remove_versions(directory: &Path, keep: Option<&str>) -> Result<(), ProblemError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        Problem::Internal.error(format!("read {}: {}", directory.display(), error))
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if keep
            .map(|name| entry.file_name().to_string_lossy() == name)
            .unwrap_or(false)
        {
            continue;
        }
        fs::remove_dir_all(&path).map_err(|error| {
            Problem::Internal.error(format!("remove {}: {}", path.display(), error))
        })?;
    }
    Ok(())
}
