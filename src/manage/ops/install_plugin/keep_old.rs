//! Keep-old install strategy: the previous version stays on disk beside the new
//! one, so a failed commit reports the error without rolling back.

use super::*;

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
pub(super) fn install_keep_old(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
    poll: &mut dyn FnMut(),
) -> Result<Service, InstallFailure> {
    // No hook-specific rollback here: keep-old keeps the previous version and
    // switches `current`, so a failing post-install just reports the error.
    // A replace install rolls back through its own strategy handling.
    //
    // The new version lives next to the old one until `current` switches, so
    // the old version's bytes cannot be reclaimed: the plan is the same as a
    // first install. Running out is the one refusal the caller can retry as
    // `replace`, so it leaves with its own category.
    let free = free_space(plugin_dir)?;
    if replace_plan(free, None, package.unpacked_size()) == ReplacePlan::NoSpace {
        return Err(InstallFailure::OutOfSpace(out_of_space(
            package, free, None,
        )));
    }
    let (version_dir, enabled) = write_version(package, plugin_dir, was_installed)?;
    copy_config(package, plugin_dir)?;
    let service = service_from_manifest(
        paths,
        limits,
        &version_dir,
        &package.manifest,
        enabled,
        interpreter,
    )?;
    run_post_install(paths, limits, package, id, &version_dir, poll)?;
    Ok(service)
}

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
pub(super) fn install_keep_old_staged(
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
    let (version_dir, enabled) = commit_staged(staged, plugin_dir, was_installed)?;
    copy_config_staged(&manifest, plugin_dir, &version_dir)?;
    let service =
        service_from_manifest(paths, limits, &version_dir, &manifest, enabled, interpreter)?;
    run_post_install_manifest(paths, limits, &manifest, id, &version_dir, poll)?;
    Ok(service)
}
