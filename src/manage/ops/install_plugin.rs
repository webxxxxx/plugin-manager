//! 插件安装生命周期：暂存、校验后的提交、回滚与空间检查。
use std::fs;
use std::path::{Path, PathBuf};

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::registry::{Interpreter, Service, RESERVED_ID};
use crate::manage as package;
use crate::manage::check::interpreter;
use crate::manage::model::layout::{current_version, plugin_dir};
use crate::manage::model::manifest::Package;
use crate::manage::ops::config::{write_config, write_hash};
use crate::manage::ops::scan::{resolve_interpreter, service_from_manifest};
use crate::manage::{
    ensure_hooks_allowed, ensure_hooks_allowed_manifest, remove_versions, replace_plan,
    ReplacePlan, Strategy,
};
use crate::platform::log;
use crate::platform::paths::Paths;
use crate::supervise::ops::hook;

/// Preflight a package that has already been streamed into a staging directory.
/// This mirrors [`verify_package`] but never holds the payload in memory.
pub fn prepare_staged(
    staged: &package::StagedPackage,
    opt_in_hooks: bool,
) -> Result<Option<Interpreter>, ProblemError> {
    package::validate_manifest(&staged.manifest)?;
    ensure_hooks_allowed_manifest(&staged.manifest, opt_in_hooks)?;
    package::validate_staged_payload(&staged.manifest, &staged.root)?;
    resolve_interpreter(&staged.manifest, interpreter::check)
}

/// Commit a streamed package. The caller must run [`prepare_staged`] first and
/// must stop the running version before calling this for `Replace`.
pub fn install_staged(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    staged: package::StagedPackage,
    strategy: Strategy,
    opt_in_hooks: bool,
    host_interpreter: Option<Interpreter>,
    poll: &mut dyn FnMut(),
) -> Result<Service, ProblemError> {
    let id = staged.manifest.name.clone();
    if id == RESERVED_ID {
        return Err(Problem::Conflict.error("the built-in svc id is reserved"));
    }
    if !package::valid_name(&id) {
        return Err(Problem::Invalid.error(format!("invalid plugin id: {}", id)));
    }
    ensure_hooks_allowed_manifest(&staged.manifest, opt_in_hooks)?;
    if paths.services_dir.join(format!("{}.json", id)).exists() {
        return Err(
            Problem::Conflict.error(format!("plugin id conflicts with a core service: {}", id))
        );
    }

    let plugin_dir = plugin_dir(paths, &id);
    let was_installed = current_version(paths, &id).is_some();
    fs::create_dir_all(&plugin_dir).map_err(|error| {
        Problem::Internal.error(format!("create {}: {}", plugin_dir.display(), error))
    })?;

    match strategy {
        Strategy::KeepOld => install_keep_old_staged(
            paths,
            limits,
            staged,
            &id,
            &plugin_dir,
            was_installed,
            host_interpreter.as_ref(),
            poll,
        ),
        Strategy::Replace => install_replace_staged(
            paths,
            limits,
            staged,
            &id,
            &plugin_dir,
            was_installed,
            host_interpreter.as_ref(),
            poll,
        ),
    }
}

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
fn install_keep_old_staged(
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
}

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
fn install_replace_staged(
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

fn commit_staged(
    staged: package::StagedPackage,
    plugin_dir: &Path,
    was_installed: bool,
) -> Result<(PathBuf, bool), ProblemError> {
    let staging_root = plugin_dir.parent().unwrap_or(plugin_dir);
    if !staged.root.starts_with(staging_root)
        && !fs::canonicalize(staging_root)
            .map(|path| staged.root.starts_with(path))
            .unwrap_or(false)
    {
        return Err(Problem::Invalid.error("staging directory is outside the plugin tree"));
    }
    let version_dir = plugin_dir.join(&staged.manifest.version);
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir).map_err(|error| {
            Problem::Internal.error(format!("remove {}: {}", version_dir.display(), error))
        })?;
    }
    fs::rename(&staged.root, &version_dir).map_err(|error| {
        Problem::Internal.error(format!(
            "rename {} -> {}: {}",
            staged.root.display(),
            version_dir.display(),
            error
        ))
    })?;
    write_current(plugin_dir, &staged.manifest.version)?;
    let enabled_marker = plugin_dir.join("enabled");
    let enabled = if was_installed {
        enabled_marker.exists()
    } else {
        false
    };
    Ok((version_dir, enabled))
}

fn copy_config_staged(
    manifest: &package::Manifest,
    plugin_dir: &Path,
    version_dir: &Path,
) -> Result<(), ProblemError> {
    let template_path = manifest.config.as_deref().unwrap_or("config/config");
    let template_path = version_dir.join(template_path);
    let template = match fs::read(&template_path) {
        Ok(data) => data,
        Err(_) => return Ok(()),
    };
    let target = plugin_dir.join("config");
    let hash_path = plugin_dir.join("config.template.sha256");
    let hash = package::hex_sha256(&template);
    if !target.exists() {
        write_config(&target, &template)?;
        write_hash(&hash_path, &hash)?;
        return Ok(());
    }
    let stored = fs::read_to_string(&hash_path)
        .ok()
        .map(|value| value.trim().to_string());
    match stored {
        None => write_hash(&hash_path, &hash)?,
        Some(stored) if stored != hash => {
            write_config(&plugin_dir.join("config.new"), &template)?;
            write_hash(&hash_path, &hash)?;
            log::svcd(&format!(
                "svcd: plugin {}: template changed; wrote config.new",
                manifest.name
            ));
        }
        Some(_) => {}
    }
    Ok(())
}

fn run_post_install_manifest(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    manifest: &package::Manifest,
    id: &str,
    version_dir: &Path,
    poll: &mut dyn FnMut(),
) -> Result<(), ProblemError> {
    let Some(path) = manifest
        .hooks
        .as_ref()
        .and_then(|hooks| hooks.post_install.as_deref())
    else {
        return Ok(());
    };
    hook::run(
        &hook::Invocation {
            id,
            version: &manifest.version,
            action: "post-install",
            version_dir,
            path,
            timeout_ms: hook::TIMEOUT_MS,
        },
        paths,
        limits,
        poll,
    )
    .map_err(|error| {
        ProblemError::new(
            error.problem,
            format!("post-install hook failed: {}", error),
        )
    })
}

/// Remove stale staging directories. Called once at daemon startup.
pub fn cleanup_staging(paths: &Paths) {
    let Ok(root) = fs::read_dir(&paths.plugins_dir) else {
        return;
    };
    for entry in root.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(".staging-") {
            let _ = fs::remove_dir_all(entry.path());
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let Ok(children) = fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.flatten() {
            let name = child.file_name();
            if name.to_string_lossy().starts_with(".staging-") {
                let _ = fs::remove_dir_all(child.path());
            }
        }
    }
}

/// What an install rejection implies for its caller.
///
/// `keep-old` needs the new version next to the old one, so running out of
/// disk is recoverable: the daemon may stop the plugin and retry as `replace`.
/// The space plan decides that here, so no upper layer reads `detail` (#151).
#[derive(Debug)]
pub enum InstallFailure {
    /// There is no room for the new version beside the old one.
    OutOfSpace(ProblemError),
    /// Any other refusal.
    Refused(ProblemError),
}

impl InstallFailure {
    pub fn into_problem(self) -> ProblemError {
        match self {
            InstallFailure::OutOfSpace(error) | InstallFailure::Refused(error) => error,
        }
    }
}

impl From<ProblemError> for InstallFailure {
    fn from(error: ProblemError) -> InstallFailure {
        InstallFailure::Refused(error)
    }
}

pub fn install_package(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    strategy: Strategy,
    opt_in_hooks: bool,
    host_interpreter: Option<Interpreter>,
    poll: &mut dyn FnMut(),
) -> Result<Service, InstallFailure> {
    let id = package.manifest.name.clone();
    if id == RESERVED_ID {
        return Err(Problem::Conflict
            .error("the built-in svc id is reserved")
            .into());
    }
    ensure_hooks_allowed(package, opt_in_hooks)?;
    if paths.services_dir.join(format!("{}.json", id)).exists() {
        return Err(Problem::Conflict
            .error(format!("plugin id conflicts with a core service: {}", id))
            .into());
    }

    let plugin_dir = plugin_dir(paths, &id);
    let was_installed = current_version(paths, &id).is_some();
    fs::create_dir_all(&plugin_dir).map_err(|error| {
        Problem::Internal.error(format!("create {}: {}", plugin_dir.display(), error))
    })?;

    match strategy {
        Strategy::KeepOld => install_keep_old(
            paths,
            limits,
            package,
            &id,
            &plugin_dir,
            was_installed,
            host_interpreter.as_ref(),
            poll,
        ),
        Strategy::Replace => install_replace(
            paths,
            limits,
            package,
            &id,
            &plugin_dir,
            was_installed,
            host_interpreter.as_ref(),
            poll,
        )
        .map_err(InstallFailure::from),
    }
}

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
fn install_keep_old(
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
        id,
        &version_dir,
        &package.manifest,
        enabled,
        interpreter,
    )?;
    run_post_install(paths, limits, package, id, &version_dir, poll)?;
    Ok(service)
}

fn run_post_install(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    id: &str,
    version_dir: &Path,
    poll: &mut dyn FnMut(),
) -> Result<(), ProblemError> {
    let Some(path) = package
        .manifest
        .hooks
        .as_ref()
        .and_then(|hooks| hooks.post_install.as_deref())
    else {
        return Ok(());
    };
    hook::run(
        &hook::Invocation {
            id,
            version: &package.manifest.version,
            action: "post-install",
            version_dir,
            path,
            timeout_ms: hook::TIMEOUT_MS,
        },
        paths,
        limits,
        poll,
    )
    .map_err(|error| {
        ProblemError::new(
            error.problem,
            format!("post-install hook failed: {}", error),
        )
    })
}

#[allow(clippy::too_many_arguments)] // The strategy split already carries every install input.
fn install_replace(
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

fn write_version(
    package: &Package,
    plugin_dir: &Path,
    was_installed: bool,
) -> Result<(PathBuf, bool), ProblemError> {
    let temp = plugin_dir.join(format!(
        "{}.tmp.{}",
        package.manifest.version,
        std::process::id()
    ));
    if temp.exists() {
        fs::remove_dir_all(&temp).map_err(|error| {
            Problem::Internal.error(format!("remove {}: {}", temp.display(), error))
        })?;
    }
    if let Err(error) = package.write_to(&temp) {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }

    let version_dir = plugin_dir.join(&package.manifest.version);
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir).map_err(|error| {
            Problem::Internal.error(format!("remove {}: {}", version_dir.display(), error))
        })?;
    }
    fs::rename(&temp, &version_dir).map_err(|error| {
        Problem::Internal.error(format!("rename {}: {}", version_dir.display(), error))
    })?;

    write_current(plugin_dir, &package.manifest.version)?;

    let enabled_marker = plugin_dir.join("enabled");
    // A fresh install starts disabled: installing a package must not silently
    // arm an autostart. The operator turns it on with `svc enable <id>` (or
    // `plugin enable <id>`); an upgrade keeps whatever the marker already says.
    let enabled = if was_installed {
        enabled_marker.exists()
    } else {
        false
    };
    Ok((version_dir, enabled))
}

fn write_current(plugin_dir: &Path, version: &str) -> Result<(), ProblemError> {
    let path = plugin_dir.join("current");
    fs::write(&path, version)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))
}

fn copy_config(package: &Package, plugin_dir: &Path) -> Result<(), ProblemError> {
    let Some(template) = package.config_template() else {
        return Ok(());
    };
    let target = plugin_dir.join("config");
    let hash_path = plugin_dir.join("config.template.sha256");
    let hash = package::hex_sha256(&template.data);
    if !target.exists() {
        write_config(&target, &template.data)?;
        write_hash(&hash_path, &hash)?;
        return Ok(());
    }
    let stored = fs::read_to_string(&hash_path)
        .ok()
        .map(|value| value.trim().to_string());
    match stored {
        None => write_hash(&hash_path, &hash)?,
        Some(stored) if stored != hash => {
            write_config(&plugin_dir.join("config.new"), &template.data)?;
            write_hash(&hash_path, &hash)?;
            log::svcd(&format!(
                "svcd: plugin {}: template changed; wrote config.new",
                package.manifest.name
            ));
        }
        Some(_) => {}
    }
    Ok(())
}

struct ConfigSnapshot {
    config: Option<Vec<u8>>,
    template_hash: Option<Vec<u8>>,
    config_new: Option<Vec<u8>>,
}

impl ConfigSnapshot {
    fn capture(plugin_dir: &Path) -> Self {
        Self {
            config: fs::read(plugin_dir.join("config")).ok(),
            template_hash: fs::read(plugin_dir.join("config.template.sha256")).ok(),
            config_new: fs::read(plugin_dir.join("config.new")).ok(),
        }
    }

    fn restore(&self, plugin_dir: &Path) -> Vec<String> {
        let mut errors = Vec::new();
        for (path, content, config) in [
            (plugin_dir.join("config"), &self.config, true),
            (
                plugin_dir.join("config.template.sha256"),
                &self.template_hash,
                false,
            ),
            (plugin_dir.join("config.new"), &self.config_new, false),
        ] {
            if let Err(error) = restore_file(&path, content, config) {
                errors.push(error.detail().to_string());
            }
        }
        errors
    }
}

fn restore_file(path: &Path, content: &Option<Vec<u8>>, config: bool) -> Result<(), ProblemError> {
    match content {
        Some(bytes) if config => write_config(path, bytes),
        Some(bytes) => fs::write(path, bytes).map_err(|error| {
            Problem::Internal.error(format!("write {}: {}", path.display(), error))
        }),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(Problem::Internal.error(format!("remove {}: {}", path.display(), error)))
            }
        },
    }
}

/// The failure a full disk produces. One message shape for both strategies.
fn out_of_space(package: &Package, free: u64, old_size: Option<u64>) -> ProblemError {
    Problem::Internal.error(format!(
        "not enough space to install {} {}: need {} bytes, {} free{}",
        package.manifest.name,
        package.manifest.version,
        package.unpacked_size(),
        free,
        old_size
            .map(|size| format!(", current version uses {size} bytes"))
            .unwrap_or_default()
    ))
}

fn directory_size(path: &Path) -> Result<u64, ProblemError> {
    let mut total = 0u64;
    let entries = fs::read_dir(path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            Problem::Internal.error(format!("read {}: {}", path.display(), error))
        })?;
        let metadata = entry.metadata().map_err(|error| {
            Problem::Internal.error(format!("stat {}: {}", entry.path().display(), error))
        })?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn free_space(path: &Path) -> Result<u64, ProblemError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            Problem::Internal.error(format!("free space: invalid path {}", path.display()))
        })?;
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
            return Err(Problem::Internal.error(format!(
                "free space {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available = 0u64;
        let mut total = 0u64;
        let mut total_free = 0u64;
        // SAFETY: The handle/pointer is produced by the immediately preceding OS call and is valid for this call.
        if unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free)
        } == 0
        {
            return Err(Problem::Internal.error(format!(
                "free space {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
        Ok(available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(Problem::Internal.error("free space is not supported on this platform"))
    }
}

pub fn root_required() -> Result<(), ProblemError> {
    if crate::platform::is_privileged() || crate::platform::paths::user_mode() {
        Ok(())
    } else {
        Err(Problem::Internal.error("root / Administrator is required"))
    }
}
