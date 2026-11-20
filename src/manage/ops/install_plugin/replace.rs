//! Replace install strategy: plan the space, move the old version aside when
//! needed, and roll the layout and config back together on failure.

use super::*;

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
pub(super) fn install_replace(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
    poll: &mut dyn FnMut(),
) -> Result<Service, ProblemError> {
    let old_version =
        current_version(paths, id).filter(|version| plugin_dir.join(version).is_dir());
    let old_dir = old_version.as_ref().map(|version| plugin_dir.join(version));
    let old_size = match &old_dir {
        Some(path) => Some(directory_size(path)?),
        None => None,
    };
    let new_size = package.unpacked_size();
    let free = free_space(plugin_dir)?;
    let plan = replace_plan(free, old_size, new_size);
    if plan == ReplacePlan::NoSpace {
        return Err(out_of_space(package, free, old_size));
    }

    let backup = if plan == ReplacePlan::Backup {
        let old_dir = old_dir.as_ref().expect("backup implies an old version");
        let old_version = old_version.as_ref().expect("backup implies an old version");
        let backup = plugin_dir.join(format!(".backup-{}.{}", old_version, std::process::id()));
        if backup.exists() {
            fs::remove_dir_all(&backup).map_err(|error| {
                Problem::Internal.error(format!("remove {}: {}", backup.display(), error))
            })?;
        }
        fs::rename(old_dir, &backup).map_err(|error| {
            Problem::Invalid.error(format!("backup {}: {}", old_dir.display(), error))
        })?;
        Some(backup)
    } else {
        if plan == ReplacePlan::Reclaim {
            if let Some(old_dir) = &old_dir {
                fs::remove_dir_all(old_dir).map_err(|error| {
                    Problem::Internal.error(format!("remove {}: {}", old_dir.display(), error))
                })?;
            }
            let _ = fs::remove_file(plugin_dir.join("current"));
        }
        None
    };

    let snapshot = ConfigSnapshot::capture(plugin_dir);
    let enabled_existed = plugin_dir.join("enabled").exists();
    let result =
        write_version(package, plugin_dir, was_installed).and_then(|(version_dir, enabled)| {
            copy_config(package, plugin_dir)?;
            let service = service_from_manifest(
                paths,
                limits,
                id,
                &version_dir,
                &package.manifest,
                enabled,
                interpreter,
            )?;
            run_post_install(paths, limits, package, id, &version_dir, poll)?;
            Ok(service)
        });

    match result {
        Ok(service) => {
            if let Some(backup) = backup {
                if let Err(error) = fs::remove_dir_all(&backup) {
                    log::svcd(&format!(
                        "svcd: plugin {}: installed, but cannot remove backup {}: {}",
                        id,
                        backup.display(),
                        error
                    ));
                }
            }
            if let Err(error) = remove_versions(plugin_dir, Some(&package.manifest.version)) {
                log::svcd(&format!(
                    "svcd: plugin {}: installed, but cannot prune old versions: {}",
                    id, error
                ));
            }
            Ok(service)
        }
        Err(error) => {
            let mut rollback = Vec::new();
            let new_version = plugin_dir.join(&package.manifest.version);
            if let Err(remove) = fs::remove_dir_all(&new_version) {
                if remove.kind() != std::io::ErrorKind::NotFound {
                    rollback.push(format!("remove {}: {}", new_version.display(), remove));
                }
            }
            if !enabled_existed {
                let marker = plugin_dir.join("enabled");
                if let Err(remove) = fs::remove_file(&marker) {
                    if remove.kind() != std::io::ErrorKind::NotFound {
                        rollback.push(format!("remove {}: {}", marker.display(), remove));
                    }
                }
            }
            rollback.extend(snapshot.restore(plugin_dir));
            if let (Some(backup), Some(old_dir), Some(old_version)) =
                (&backup, &old_dir, &old_version)
            {
                match fs::rename(backup, old_dir) {
                    Ok(()) => {
                        if let Err(restore) = write_current(plugin_dir, old_version) {
                            rollback.push(restore.detail().to_string());
                        }
                    }
                    Err(restore) => {
                        rollback.push(format!("restore {}: {}", old_dir.display(), restore));
                    }
                }
            } else if backup.is_none() {
                let current = plugin_dir.join("current");
                if let Err(remove) = fs::remove_file(&current) {
                    if remove.kind() != std::io::ErrorKind::NotFound {
                        rollback.push(format!("remove {}: {}", current.display(), remove));
                    }
                }
            }
            if rollback.is_empty() {
                Err(error)
            } else {
                Err(Problem::Internal.error(format!(
                    "{} (rollback incomplete: {})",
                    error,
                    rollback.join("; ")
                )))
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
pub(super) fn install_replace_staged(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    staged: package::StagedPackage,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
    poll: &mut dyn FnMut(),
) -> Result<Service, ProblemError> {
    let manifest = staged.manifest.clone();
    let old_version =
        current_version(paths, id).filter(|version| plugin_dir.join(version).is_dir());
    let old_dir = old_version.as_ref().map(|version| plugin_dir.join(version));
    let backup = old_version
        .as_ref()
        .map(|version| plugin_dir.join(format!(".backup-{}.{}", version, std::process::id())));
    if let (Some(old_dir), Some(backup)) = (&old_dir, &backup) {
        if backup.exists() {
            fs::remove_dir_all(backup).map_err(|error| {
                Problem::Internal.error(format!("remove {}: {}", backup.display(), error))
            })?;
        }
        fs::rename(old_dir, backup).map_err(|error| {
            Problem::Invalid.error(format!("backup {}: {}", old_dir.display(), error))
        })?;
    }
    let enabled_existed = plugin_dir.join("enabled").exists();
    let result = (|| {
        let (version_dir, enabled) = commit_staged(staged, plugin_dir, was_installed)?;
        copy_config_staged(&manifest, plugin_dir, &version_dir)?;
        let service = service_from_manifest(
            paths,
            limits,
            id,
            &version_dir,
            &manifest,
            enabled,
            interpreter,
        )?;
        run_post_install_manifest(paths, limits, &manifest, id, &version_dir, poll)?;
        Ok(service)
    })();
    match result {
        Ok(service) => {
            if let Some(backup) = backup {
                let _ = fs::remove_dir_all(backup);
            }
            let _ = remove_versions(plugin_dir, Some(&manifest.version));
            Ok(service)
        }
        Err(error) => {
            let new_version = plugin_dir.join(&manifest.version);
            let _ = fs::remove_dir_all(&new_version);
            if !enabled_existed {
                let _ = fs::remove_file(plugin_dir.join("enabled"));
            }
            if let (Some(backup), Some(old_dir), Some(old_version)) =
                (&backup, &old_dir, &old_version)
            {
                let _ = fs::rename(backup, old_dir);
                let _ = write_current(plugin_dir, old_version);
            }
            Err(error)
        }
    }
}
