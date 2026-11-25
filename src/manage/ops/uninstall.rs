//! 卸载：删版本目录、清日志。
use std::fs;
use std::time::Duration;

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::hook::{HookSpec, Hooks, TIMEOUT_MS};
use crate::kernel::layout::plugin_dir;
use crate::kernel::registry::RESERVED_ID;
use crate::manage::ops::export_plugin::remove_versions;
use crate::platform::paths::Paths;

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
    id: &str,
    purge: bool,
    force: bool,
    hooks: &mut dyn Hooks,
) -> Result<(), ProblemError> {
    if !crate::kernel::registry::valid_id(id) || id == RESERVED_ID {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    // The plugin may sit on a volume that is not there right now. Removing the
    // record without the files would leave an orphan copy that reappears when
    // the card comes back, so the default answer is "put the volume back";
    // `--force` is the operator saying they know the volume is gone.
    if let Some(root) = crate::platform::locations::root_of(paths, id) {
        if !crate::platform::locations::available(&root) {
            if !force {
                return Err(Problem::Conflict.error(format!(
                    "plugin {} lives in {}, which is not available right now; put the volume back, or `plugin uninstall --force {}` to forget the record and leave the volume untouched",
                    id,
                    root.display(),
                    id
                )));
            }
            forget_location(paths, id);
            // Audit line: nothing on the volume changed, and the next scan will
            // not re-add this plugin because the record is what said it exists.
            info!(
                "plugin {}: location record forgotten (root {} is not available); the volume was left untouched",
                id,
                root.display()
            );
            return Ok(());
        }
    }
    let directory = plugin_dir(paths, id);
    if !directory.is_dir() {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    run_pre_remove(paths, id, hooks);
    if purge {
        fs::remove_dir_all(&directory).map_err(|error| {
            Problem::Internal.error(format!("remove {}: {}", directory.display(), error))
        })?;
        forget_location(paths, id);
        return Ok(());
    }

    // Keep the user's config and the plugin's own data directory: reinstalling
    // the same plugin finds its settings, host keys and geo files again. Only
    // `--purge` (above) removes everything.
    remove_versions(&directory, &["config", "data"])?;
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
    // The record is what makes a plugin "installed" for `plugin where` and for
    // root selection; an uninstalled plugin must not keep pinning a root.
    forget_location(paths, id);
    Ok(())
}

/// Drop one plugin's location record. A failure only means the next scan keeps
/// a stale entry for a moment, so it warns instead of failing an uninstall that
/// already removed the files.
fn forget_location(paths: &Paths, id: &str) {
    if let Err(error) = crate::platform::locations::forget(paths, id) {
        warn!(
            "plugin {}: cannot forget the location record: {}",
            id, error
        );
    }
}

/// A failing pre-remove hook must not block uninstall; the contract only
/// requires a log line so a broken package can still be removed.
fn run_pre_remove(paths: &Paths, id: &str, hooks: &mut dyn Hooks) {
    let Some(version) = crate::manage::current_version(paths, id) else {
        return;
    };
    let version_dir = plugin_dir(paths, id).join(&version);
    let manifest = match crate::manage::ops::scan::read_manifest(&version_dir) {
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
    if let Err(error) = hooks.run(&HookSpec {
        id,
        version: &version,
        action: "pre-remove",
        version_dir: &version_dir,
        path,
        timeout_ms: TIMEOUT_MS,
    }) {
        warn!(
            "plugin {}: pre-remove hook failed; continuing uninstall: {}",
            id, error
        );
    }
}
