//! 卸载：删版本目录、清日志。
use std::fs;
use std::time::Duration;

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::registry::RESERVED_ID;
use crate::manage as package;
use crate::manage::model::layout::{current_version, plugin_dir};
use crate::manage::ops::export_plugin::remove_versions;
use crate::manage::ops::scan::read_manifest;
use crate::platform::paths::Paths;
use crate::supervise::ops::hook;

/// Remove the service and install-hook logs for a purged plugin. The slot is
/// already gone, but its reader threads may still be closing the file on
/// Windows, so retry briefly instead of leaving rotated logs behind.
pub fn remove_logs(paths: &Paths, id: &str) {
    let directory = &paths.logs_dir;
    let names = [
        format!("{}.log", id),
        format!("{}.log.1", id),
        format!("{}.log.2", id),
        format!("{}-hooks.log", id),
        format!("{}-hooks.log.1", id),
        format!("{}-hooks.log.2", id),
    ];
    for name in names {
        let path = directory.join(name);
        for _ in 0..10 {
            match fs::remove_file(&path) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
}

pub fn uninstall(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    id: &str,
    purge: bool,
    poll: &mut dyn FnMut(),
) -> Result<(), ProblemError> {
    if !package::valid_name(id) || id == RESERVED_ID {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    let directory = plugin_dir(paths, id);
    if !directory.is_dir() {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    run_pre_remove(paths, limits, id, poll);
    if purge {
        fs::remove_dir_all(&directory).map_err(|error| {
            Problem::Internal.error(format!("remove {}: {}", directory.display(), error))
        })?;
        return Ok(());
    }

    remove_versions(&directory, Some("config"))?;
    for name in ["current", "enabled"] {
        let path = directory.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Problem::Internal.error(format!(
                    "remove {}: {}",
                    path.display(),
                    error
                )))
            }
        }
    }
    Ok(())
}

/// A failing pre-remove hook must not block uninstall; the spec only requires a
/// log line so a broken package can still be removed.
fn run_pre_remove(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    id: &str,
    poll: &mut dyn FnMut(),
) {
    let Some(version) = current_version(paths, id) else {
        return;
    };
    let version_dir = plugin_dir(paths, id).join(&version);
    let manifest = match read_manifest(&version_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            warn!("plugin {}: skipped pre-remove hook: {}", id, error);
            return;
        }
    };
    let Some(path) = manifest
        .hooks
        .as_ref()
        .and_then(|hooks| hooks.pre_remove.as_deref())
    else {
        return;
    };
    let result = hook::run(
        &hook::Invocation {
            id,
            version: &version,
            action: "pre-remove",
            version_dir: &version_dir,
            path,
            timeout_ms: hook::TIMEOUT_MS,
        },
        paths,
        limits,
        poll,
    );
    if let Err(error) = result {
        warn!(
            "plugin {}: pre-remove hook failed; continuing uninstall: {}",
            id, error
        );
    }
}
