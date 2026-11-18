//! Plugin installation layout and the `plugin` scan used by the daemon.
//!
//! The package is verified by [`crate::manage::package`] before this module touches the
//! plugin tree. Versions live under `<data>/plugins/<id>/<version>/`; `current`
//! is a version text file and `enabled` is a marker.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::platform::compat;
use crate::supervise::ops::hook;
use crate::manage::interpreter;
use crate::kernel::contract::{Capability as KernelCapability, CapabilityName, Effect};
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::platform::log;
use crate::manage::package::{self, Package};
use crate::platform::paths::Paths;
use crate::kernel::registry::{
    Exec, Hook, Interpreter, Kind, Lifecycle, Ready, Restart, Service, State, RESERVED_ID,
};

/// Serialise config writes inside this process. The on-disk `config.lock`
/// covers other processes on Unix; this mutex also covers Windows, where
/// `flock` is a no-op, and keeps two threads from sharing the same
/// `config.tmp.<pid>` path.
static CONFIG_WRITES: Mutex<()> = Mutex::new(());

pub(crate) fn config_lock() -> MutexGuard<'static, ()> {
    CONFIG_WRITES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    KeepOld,
    Replace,
}

impl Strategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "keep-old" => Some(Strategy::KeepOld),
            "replace" => Some(Strategy::Replace),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplacePlan {
    Fresh,
    Backup,
    Reclaim,
    NoSpace,
}

fn replace_plan(free: u64, old: Option<u64>, new: u64) -> ReplacePlan {
    match old {
        None if free >= new => ReplacePlan::Fresh,
        None => ReplacePlan::NoSpace,
        Some(_) if free >= new => ReplacePlan::Backup,
        Some(old) if free.saturating_add(old) >= new => ReplacePlan::Reclaim,
        Some(_) => ReplacePlan::NoSpace,
    }
}

/// Hooks run with the installer's privileges: a desktop build (the default
/// feature set) allows them, an embedded build (`--no-default-features`) only
/// with the explicit opt-in. `opt_in_hooks` is that escape hatch, so `false`
/// does not mean "refuse" on desktop.
fn hooks_allowed(opt_in_hooks: bool) -> bool {
    opt_in_hooks || cfg!(feature = "http")
}

/// Reject a hook package before any install side effect (such as stopping the
/// running version) can happen.
pub fn ensure_hooks_allowed(package: &Package, opt_in_hooks: bool) -> Result<(), ProblemError> {
    ensure_hooks_allowed_manifest(&package.manifest, opt_in_hooks)
}

pub fn ensure_hooks_allowed_manifest(
    manifest: &package::Manifest,
    opt_in_hooks: bool,
) -> Result<(), ProblemError> {
    if package::declares_hooks(manifest) && !hooks_allowed(opt_in_hooks) {
        return Err(Problem::Invalid.error(format!(
            "package {} declares hooks; this build requires --allow-hooks",
            manifest.name
        )));
    }
    Ok(())
}

/// Validate everything that can fail before the plugin tree is touched and
/// resolve the host interpreter. The daemon runs this before stopping a running
/// version, so a bad package or missing interpreter cannot leave the old
/// version stopped.
pub fn prepare(package: &Package) -> Result<Option<Interpreter>, ProblemError> {
    package::validate_manifest(&package.manifest)?;
    let files: Vec<(&str, &[u8])> = package
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.data.as_slice()))
        .collect();
    package::validate_script_payload(&package.manifest, &files)?;
    resolve_interpreter(&package.manifest, interpreter::check)
}

/// A package that passed every install-time check, plus the host state the
/// install will use. Produced by [`verify`].
pub struct Verified {
    pub package: Package,
    /// False when the insecure fallback accepted a package whose signature was
    /// missing or untrusted; the caller must log that fact on install.
    pub signature_verified: bool,
    pub interpreter: Option<Interpreter>,
}

/// Run every check an install performs before it touches the plugin tree:
/// package signature and file hashes, host compatibility, hook policy and the
/// interpreter lookup. It has no side effects, so the daemon can run it before
/// stopping a running version.
pub fn verify(
    paths: &Paths,
    path: &Path,
    allow_insecure: bool,
    opt_in_hooks: bool,
) -> Result<Verified, ProblemError> {
    let (package, signature_verified) =
        package::verify_package_with_fallback(path, allow_insecure, &paths.trust_dir)?;
    verify_package(paths, package, signature_verified, opt_in_hooks)
}

fn verify_package(
    paths: &Paths,
    package: Package,
    signature_verified: bool,
    opt_in_hooks: bool,
) -> Result<Verified, ProblemError> {
    let devices = package
        .manifest
        .compat
        .as_ref()
        .map(|spec| spec.devices.clone())
        .unwrap_or_default();
    if let Some(reason) = compat::verdict_reason(compat::evaluate(
        crate::platform::target(),
        &devices,
        &compat::detect(paths, MAX_SCHEMA, MAX_PROTOCOL),
    )) {
        // A restricted package on the wrong device generation is the same
        // family as a host triple mismatch: the host cannot run it (400).
        return Err(Problem::UnsupportedPlatform.error(reason));
    }
    ensure_hooks_allowed(&package, opt_in_hooks)?;
    let interpreter = prepare(&package)?;
    Ok(Verified {
        package,
        signature_verified,
        interpreter,
    })
}

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
        ),
        Strategy::Replace => install_replace_staged(
            paths,
            limits,
            staged,
            &id,
            &plugin_dir,
            was_installed,
            host_interpreter.as_ref(),
        ),
    }
}

fn install_keep_old_staged(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    staged: package::StagedPackage,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
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
    run_post_install_manifest(paths, limits, &manifest, id, &version_dir)?;
    Ok(service)
}

fn install_replace_staged(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    staged: package::StagedPackage,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
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
        run_post_install_manifest(paths, limits, &manifest, id, &version_dir)?;
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
        &mut hook::poll_this_loop,
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

/// Allocate a unique staging directory under the plugin's own tree so the
/// final commit is a same-filesystem rename.
#[allow(dead_code)]
pub fn staging_dir(paths: &Paths, id: &str) -> Result<PathBuf, ProblemError> {
    if !package::valid_name(id) {
        return Err(Problem::Invalid.error(format!("invalid plugin id: {}", id)));
    }
    let directory = plugin_dir(paths, id);
    fs::create_dir_all(&directory).map_err(|error| {
        Problem::Internal.error(format!("create {}: {}", directory.display(), error))
    })?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(directory.join(format!(".staging-{}-{}", std::process::id(), nanos)))
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
        ),
        Strategy::Replace => install_replace(
            paths,
            limits,
            package,
            &id,
            &plugin_dir,
            was_installed,
            host_interpreter.as_ref(),
        )
        .map_err(InstallFailure::from),
    }
}

fn install_keep_old(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
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
    run_post_install(paths, limits, package, id, &version_dir)?;
    Ok(service)
}

fn run_post_install(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    id: &str,
    version_dir: &Path,
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
        &mut hook::poll_this_loop,
    )
    .map_err(|error| {
        ProblemError::new(
            error.problem,
            format!("post-install hook failed: {}", error),
        )
    })
}

fn install_replace(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    package: &Package,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
    interpreter: Option<&Interpreter>,
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
            run_post_install(paths, limits, package, id, &version_dir)?;
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
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
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

pub fn scan(paths: &Paths, limits: &crate::platform::limits::Limits) -> (Vec<Service>, Vec<String>) {
    let root = &paths.plugins_dir;
    let host = crate::platform::compat::detect(paths, MAX_SCHEMA, MAX_PROTOCOL);
    let mut services = Vec::new();
    let mut warnings = Vec::new();
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (services, warnings),
        Err(error) => {
            warnings.push(format!("cannot read {}: {}", root.display(), error));
            return (services, warnings);
        }
    };

    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        if !package::valid_name(&id) || id == RESERVED_ID {
            warnings.push(format!("ignoring plugin directory {}", id));
            continue;
        }
        let directory = entry.path();
        let version = match current_version(paths, &id) {
            Some(version) => version,
            None => {
                warnings.push(format!("{}: missing current pointer", id));
                continue;
            }
        };
        let version_dir = directory.join(&version);
        let manifest = match read_manifest(&version_dir) {
            Ok(manifest) => manifest,
            Err(error) => {
                warnings.push(format!("{}: {}", id, error));
                continue;
            }
        };
        let enabled = directory.join("enabled").exists();
        let interpreter = match resolve_interpreter(&manifest, interpreter::lookup) {
            Ok(interpreter) => interpreter,
            Err(error) => {
                warnings.push(format!("{}: {}", id, error));
                continue;
            }
        };
        match service_from_manifest(
            paths,
            limits,
            &id,
            &version_dir,
            &manifest,
            enabled,
            interpreter.as_ref(),
        ) {
            Ok(mut service) => {
                let devices = service
                    .compat
                    .as_ref()
                    .map(|spec| spec.devices.clone())
                    .unwrap_or_default();
                match crate::platform::compat::evaluate(crate::platform::target(), &devices, &host) {
                    crate::platform::compat::Verdict::Compatible => services.push(service),
                    verdict => {
                        let reason = crate::platform::compat::verdict_reason(verdict)
                            .unwrap_or_else(|| "compat".into());
                        warnings.push(format!("{}: {}", id, reason));
                        service.state = State::Failed;
                        services.push(service);
                    }
                }
            }
            Err(error) => warnings.push(format!("{}: {}", id, error)),
        }
    }
    (services, warnings)
}

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
) -> Result<(), ProblemError> {
    if !package::valid_name(id) || id == RESERVED_ID {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    let directory = plugin_dir(paths, id);
    if !directory.is_dir() {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    run_pre_remove(paths, limits, id);
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
fn run_pre_remove(paths: &Paths, limits: &crate::platform::limits::Limits, id: &str) {
    let Some(version) = current_version(paths, id) else {
        return;
    };
    let version_dir = plugin_dir(paths, id).join(&version);
    let manifest = match read_manifest(&version_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            log::svcd(&format!(
                "svcd: plugin {}: skipped pre-remove hook: {}",
                id, error
            ));
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
        &mut hook::poll_this_loop,
    );
    if let Err(error) = result {
        log::svcd(&format!(
            "svcd: plugin {}: pre-remove hook failed; continuing uninstall: {}",
            id, error
        ));
    }
}

pub fn plugin_dir(paths: &Paths, id: &str) -> PathBuf {
    paths.plugins_dir.join(id)
}

pub fn current_version(paths: &Paths, id: &str) -> Option<String> {
    let text = fs::read_to_string(plugin_dir(paths, id).join("current")).ok()?;
    let version = text.trim();
    if version.is_empty()
        || version.split('.').count() != 3
        || version
            .split('.')
            .any(|part| part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()))
    {
        None
    } else {
        Some(version.to_string())
    }
}

fn service_from_manifest(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
    id: &str,
    version_dir: &Path,
    manifest: &package::Manifest,
    enabled: bool,
    interpreter: Option<&Interpreter>,
) -> Result<Service, ProblemError> {
    if manifest.platform != crate::platform::target() {
        return Err(Problem::UnsupportedPlatform.error(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            crate::platform::target()
        )));
    }
    package::check_path(&manifest.exec.path)?;
    let exec_path = version_dir.join(&manifest.exec.path);
    if !exec_path.is_file() {
        return Err(Problem::Invalid.error(format!("exec is missing: {}", exec_path.display())));
    }
    let ready = match manifest.exec.ready.as_deref() {
        None | Some("notify") => Ready::Notify,
        Some("exec") => Ready::Exec,
        Some(other) => return Err(Problem::Invalid.error(format!("unknown ready mode: {}", other))),
    };
    let mut caps: Vec<KernelCapability> = Vec::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty()
            || capability.name.starts_with("svc.")
            || capability.name == "start"
            || caps.iter().any(|cap| cap.name.as_str() == capability.name)
        {
            return Err(
                Problem::Invalid.error(format!("invalid capability name: {}", capability.name))
            );
        }
        caps.push(KernelCapability {
            name: CapabilityName::new(capability.name.clone()),
            desc: capability.desc.clone(),
            input_schema: Some(capability.input_schema.clone()),
            effect: Effect::Write,
            idempotent: false,
        });
    }
    if ready == Ready::Exec && !caps.is_empty() {
        return Err(Problem::Invalid
            .error("exec.ready=exec cannot declare capabilities: no protocol channel"));
    }
    let contract = crate::kernel::registry::contract_for(
        id,
        &manifest.version,
        caps,
        crate::kernel::registry::Carrier::Slot,
        limits,
    );

    let schema_two = manifest.schema >= 2;
    let exec_is_script = !is_native_file(&exec_path)?;
    let mut any_script = exec_is_script;
    if schema_two && exec_is_script && interpreter.is_none() {
        return Err(Problem::Invalid.error(format!(
            "plugin {} runs a script but the manifest declares no interpreter",
            id
        )));
    }

    let lifecycle = match &manifest.lifecycle {
        None => None,
        Some(lifecycle) => {
            let start = convert_lifecycle_hook(
                "start",
                version_dir,
                lifecycle.start.as_ref(),
                crate::kernel::registry::HOOK_START_TIMEOUT_MS,
                crate::kernel::registry::HOOK_TIMEOUT_MAX_MS,
                interpreter,
                &mut any_script,
            )?;
            let stop = convert_lifecycle_hook(
                "stop",
                version_dir,
                lifecycle.stop.as_ref(),
                crate::kernel::registry::HOOK_STOP_TIMEOUT_MS,
                crate::kernel::registry::HOOK_TIMEOUT_MAX_MS,
                interpreter,
                &mut any_script,
            )?;
            let status = convert_lifecycle_hook(
                "status",
                version_dir,
                lifecycle.status.as_ref(),
                crate::kernel::registry::HOOK_STATUS_TIMEOUT_MS,
                crate::kernel::registry::HOOK_STATUS_TIMEOUT_MAX_MS,
                interpreter,
                &mut any_script,
            )?;
            Some(Lifecycle {
                start,
                stop,
                status,
            })
        }
    };
    if schema_two && !any_script && interpreter.is_some() {
        return Err(Problem::Invalid.error(format!(
            "plugin {} declares an interpreter but has no script payload",
            id
        )));
    }

    let config = {
        let path = plugin_dir(paths, id).join("config");
        path.is_file().then(|| path.to_string_lossy().to_string())
    };

    Ok(Service {
        id: id.to_string(),
        kind: Kind::Plugin,
        version: manifest.version.clone(),
        protocol: manifest.protocol,
        desc: manifest.desc.clone(),
        deps: manifest.dependencies.clone(),
        contract,
        impl_ref: None,
        exec: Some(Exec {
            path: exec_path.to_string_lossy().to_string(),
            args: manifest.exec.args.clone(),
            env: manifest.exec.env.clone(),
            ready,
            interpreter: if schema_two && exec_is_script {
                interpreter.cloned()
            } else {
                None
            },
        }),
        lifecycle,
        enabled,
        // The manifest has no policy field yet; plugins keep the historical
        // on-failure behaviour and stop at Failed after the retry budget.
        restart: Restart::OnFailure,
        remain_after_exit: false,
        compat: manifest.compat.clone(),
        state: State::Stopped,
        pid: None,
        since: None,
        ready_since: None,
        next_at: None,
        failures: 0,
        last_exit: None,
        config,
    })
}

fn convert_lifecycle_hook(
    name: &str,
    version_dir: &Path,
    hook: Option<&package::Hook>,
    default_timeout_ms: u64,
    max_timeout_ms: u64,
    interpreter: Option<&Interpreter>,
    any_script: &mut bool,
) -> Result<Option<Hook>, ProblemError> {
    let Some(hook) = hook else {
        return Ok(None);
    };
    let path = version_dir.join(&hook.path);
    if !path.is_file() {
        return Err(Problem::Invalid.error(format!(
            "lifecycle.{} is missing: {}",
            name,
            path.display()
        )));
    }
    let is_script = !is_native_file(&path)?;
    if is_script {
        *any_script = true;
        if interpreter.is_none() {
            return Err(Problem::Invalid.error(format!(
                "lifecycle.{} is a script but the manifest declares no interpreter",
                name
            )));
        }
    }
    let timeout_ms = match hook.timeout {
        Some(seconds) if seconds >= 1 && seconds <= max_timeout_ms / 1000 => seconds * 1000,
        Some(seconds) => {
            return Err(Problem::Invalid.error(format!(
                "lifecycle.{} timeout must be between 1 and {} seconds, got {}",
                name,
                max_timeout_ms / 1000,
                seconds
            )))
        }
        None => default_timeout_ms,
    };
    Ok(Some(Hook {
        path: path.to_string_lossy().to_string(),
        args: hook.args.clone(),
        env: hook.env.clone(),
        timeout_ms,
        interpreter: if is_script {
            interpreter.cloned()
        } else {
            None
        },
    }))
}

/// Read only the header: installed payloads can be large native binaries.
fn is_native_file(path: &Path) -> Result<bool, ProblemError> {
    use std::io::Read;
    let mut file = fs::File::open(path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    let mut head = [0u8; 8];
    let read = file
        .read(&mut head)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    Ok(package::is_native_binary(&head[..read]))
}

fn resolve_interpreter(
    manifest: &package::Manifest,
    resolve: fn(&interpreter::Spec) -> Result<interpreter::Resolved, ProblemError>,
) -> Result<Option<Interpreter>, ProblemError> {
    let Some(spec) = manifest.interpreter.as_ref().map(|spec| interpreter::Spec {
        name: spec.name.clone(),
        args: spec.args.clone(),
        version: spec.version.clone(),
    }) else {
        return Ok(None);
    };
    let resolved = resolve(&spec)?;
    Ok(Some(Interpreter {
        path: resolved.path.to_string_lossy().to_string(),
        args: resolved.args,
    }))
}

fn read_manifest(version_dir: &Path) -> Result<package::Manifest, ProblemError> {
    let bytes = fs::read(version_dir.join("manifest.json"))
        .map_err(|error| Problem::Internal.error(format!("read manifest: {}", error)))?;
    let manifest = package::parse_manifest(&bytes)?;
    if manifest.platform != crate::platform::target() {
        return Err(Problem::UnsupportedPlatform.error(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            crate::platform::target()
        )));
    }
    Ok(manifest)
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

fn write_hash(path: &Path, hash: &str) -> Result<(), ProblemError> {
    fs::write(path, hash)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o640));
    }
    Ok(())
}

pub fn lock_config(paths: &Paths, id: &str) -> Result<fs::File, ProblemError> {
    let path = plugin_dir(paths, id).join("config.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| Problem::Internal.error(format!("open {}: {}", path.display(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(Problem::Internal.error(format!(
                "lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(file)
}

pub fn write_config(path: &Path, data: &[u8]) -> Result<(), ProblemError> {
    fs::write(path, data)
        .map_err(|error| Problem::Internal.error(format!("write {}: {}", path.display(), error)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640)).map_err(|error| {
            Problem::Internal.error(format!("chmod {}: {}", path.display(), error))
        })?;
    }
    Ok(())
}

pub fn active_config(paths: &Paths, id: &str) -> Option<PathBuf> {
    let path = plugin_dir(paths, id).join("config");
    path.is_file().then_some(path)
}

/// Where an installed plugin's declared web UI lives on disk.
#[derive(Clone, Debug)]
#[cfg(feature = "http")]
pub struct WebAssets {
    /// `payload/web/` inside the current version directory.
    pub root: PathBuf,
    /// Entry file relative to `root`; defaults to `index.html`.
    pub entry: String,
}

/// Resolve the web UI declared by `manifest.ui`.
///
/// Only `web` and `gui` UIs are browser-loadable (the desktop WebView loads the
/// same files); `headerless` and unknown types have no UI to serve. The entry is
/// relative to `payload/web/` and defaults to `index.html`.
#[cfg(feature = "http")]
pub fn web_assets(paths: &Paths, id: &str) -> Option<WebAssets> {
    let version = current_version(paths, id)?;
    let version_dir = plugin_dir(paths, id).join(version);
    let manifest = read_manifest(&version_dir).ok()?;
    let ui = manifest.ui.as_ref()?;
    if ui.kind != "web" && ui.kind != "gui" {
        return None;
    }
    Some(WebAssets {
        root: version_dir.join("payload/web"),
        entry: ui.entry.clone().unwrap_or_else(|| "index.html".to_string()),
    })
}

pub fn config_template(paths: &Paths, id: &str) -> Option<(Vec<u8>, package::Manifest)> {
    let version = current_version(paths, id)?;
    let version_dir = plugin_dir(paths, id).join(version);
    let manifest = read_manifest(&version_dir).ok()?;
    let config = manifest.config.as_deref().unwrap_or("config/config");
    let data = fs::read(version_dir.join(config)).ok()?;
    Some((data, manifest))
}

pub fn config_check(paths: &Paths, id: &str, candidate: &Path) -> Result<(), ProblemError> {
    let Some(version) = current_version(paths, id) else {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    };
    let version_dir = plugin_dir(paths, id).join(&version);
    let manifest = read_manifest(&version_dir)?;
    let Some(check) = manifest.config_check else {
        return Ok(());
    };
    package::check_path(&check.path)?;
    let program = version_dir.join(&check.path);
    if !program.is_file() {
        return Err(
            Problem::Conflict.error(format!("configCheck is missing: {}", program.display()))
        );
    }
    let mut command = check_command(&program);
    command
        .args(&check.args)
        .current_dir(&version_dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").unwrap_or_default(),
        )
        .env("WINDIR", std::env::var_os("WINDIR").unwrap_or_default())
        .env("SVCD_PLUGIN_ID", id)
        .env("SVCD_PLUGIN_VERSION", &version)
        .env("SVCD_DATA_DIR", &paths.data_dir)
        .env("SVCD_CONFIG_DIR", &paths.config_dir)
        .env("SVCD_CONFIG", candidate)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| Problem::Conflict.error(format!("spawn configCheck: {}", error)))?;
    let _ = crate::platform::host::process::adopt(&child);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = String::new();
                if let Some(mut handle) = child.stderr.take() {
                    use std::io::Read;
                    let _ = handle.read_to_string(&mut stderr);
                }
                return Err(Problem::Conflict.error(format!(
                    "configCheck failed ({}): {}",
                    status,
                    stderr.trim()
                )));
            }
            Ok(None) => {}
            Err(error) => return Err(Problem::Conflict.error(format!("configCheck: {}", error))),
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Problem::Conflict.error("configCheck timed out after 10s"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn check_command(program: &Path) -> Command {
    let extension = program
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "cmd" || extension == "bat" {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command = Command::new(shell);
        command.arg("/C").arg(program);
        return command;
    }
    Command::new(program)
}

#[cfg(not(windows))]
fn check_command(program: &Path) -> Command {
    Command::new(program)
}

pub fn replace_config(
    paths: &Paths,
    id: &str,
    candidate: &[u8],
    check: bool,
) -> Result<(), ProblemError> {
    let directory = plugin_dir(paths, id);
    if !directory.is_dir() {
        return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
    }
    if candidate.len() > 1024 * 1024 {
        return Err(Problem::TooLarge.error("config exceeds 1 MiB"));
    }
    let target = directory.join("config");
    let temporary = directory.join(format!("config.tmp.{}", std::process::id()));
    write_config(&temporary, candidate)?;
    if check {
        if let Err(error) = config_check(paths, id, &temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    if target.exists() {
        fs::copy(&target, directory.join("config.last-good"))
            .map_err(|error| Problem::Internal.error(format!("save last-good: {}", error)))?;
    }
    fs::rename(&temporary, &target)
        .map_err(|error| Problem::Internal.error(format!("replace config: {}", error)))?;
    Ok(())
}

pub fn root_required() -> Result<(), ProblemError> {
    if crate::platform::is_privileged() || crate::platform::paths::user_mode() {
        Ok(())
    } else {
        Err(Problem::Internal.error("root / Administrator is required"))
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct Backup {
    pub schema: u32,
    pub id: String,
    pub version: String,
    #[serde(rename = "configSha256")]
    pub config_sha256: Option<String>,
}

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

fn remove_versions(directory: &Path, keep: Option<&str>) -> Result<(), ProblemError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    fn root() -> &'static Path {
        let path = ROOT.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = crate::test_support::temp_dir().join(format!(
                "svcd-config-unit-{}-{}",
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(path.join("data/plugins")).expect("data");
            path
        });
        path
    }

    /// The directory set for the shared plugin tree: no process environment.
    fn paths() -> &'static Paths {
        static PATHS: OnceLock<Paths> = OnceLock::new();
        PATHS.get_or_init(|| Paths::under(root()))
    }

    fn make_plugin(id: &str, config: &str, check: bool) {
        let directory = root().join("data/plugins").join(id);
        let version_dir = directory.join("1.0.0");
        std::fs::create_dir_all(version_dir.join("config")).expect("version");
        std::fs::write(directory.join("current"), "1.0.0").expect("current");
        std::fs::write(version_dir.join("config/config"), config).expect("template");
        write_config(&directory.join("config"), config.as_bytes()).expect("active");
        let mut manifest = serde_json::json!({
            "schema": 1,
            "name": id,
            "version": "1.0.0",
            "desc": "config test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "config/config"},
            "files": []
        });
        if check {
            let path = if cfg!(windows) {
                "check/check.cmd"
            } else {
                "check/check.sh"
            };
            let script = if cfg!(windows) {
                "@echo off\r\nfindstr /C:\"good\" \"%SVCD_CONFIG%\" >nul\r\nexit /b %errorlevel%\r\n"
            } else {
                "#!/bin/sh\ngrep -q good \"$SVCD_CONFIG\"\n"
            };
            std::fs::create_dir_all(version_dir.join("check")).expect("check dir");
            std::fs::write(version_dir.join(path), script).expect("check script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    version_dir.join(path),
                    std::fs::Permissions::from_mode(0o755),
                )
                .expect("chmod check");
            }
            manifest["configCheck"] = serde_json::json!({"path": path});
        }
        std::fs::write(
            version_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("write manifest");
    }

    #[test]
    fn scan_builds_the_contract_from_manifest_capabilities() {
        let id = "contract-caps";
        make_plugin(id, "", false);
        let manifest_path = root()
            .join("data/plugins")
            .join(id)
            .join("1.0.0")
            .join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest"))
                .expect("manifest json");
        manifest["capabilities"] = serde_json::json!([{
            "name": "run",
            "desc": "run it",
            "inputSchema": {"type": "object"}
        }]);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("write manifest");

        let (services, _) = scan(paths(), &crate::platform::limits::test_default());
        let service = services
            .iter()
            .find(|service| service.id == id)
            .expect("plugin is scanned");
        let surface = service.contract.surface();
        assert_eq!(surface.len(), 1);
        assert_eq!(surface[0].name.as_str(), "run");
        assert_eq!(surface[0].desc, "run it");
        assert_eq!(surface[0].effect, Effect::Write);
        assert!(!surface[0].idempotent);
        assert!(!service
            .contract
            .failures
            .contains(&crate::kernel::failure::Class::Cancelled));
    }

    #[test]
    fn hooks_are_allowed_by_default_only_with_the_http_feature() {
        assert!(hooks_allowed(true));
        assert_eq!(hooks_allowed(false), cfg!(feature = "http"));
    }

    #[test]
    fn replace_plan_decides_from_free_old_and_new_sizes() {
        // Fresh installs need the whole package to fit.
        assert_eq!(replace_plan(100, None, 100), ReplacePlan::Fresh);
        assert_eq!(replace_plan(99, None, 100), ReplacePlan::NoSpace);
        // A backup is only safe while the new copy also fits next to the old one.
        assert_eq!(replace_plan(100, Some(80), 100), ReplacePlan::Backup);
        assert_eq!(replace_plan(150, Some(1), 100), ReplacePlan::Backup);
        // Reclaiming the old version is the fallback when both copies cannot coexist.
        assert_eq!(replace_plan(99, Some(80), 100), ReplacePlan::Reclaim);
        assert_eq!(replace_plan(20, Some(80), 100), ReplacePlan::Reclaim);
        assert_eq!(replace_plan(19, Some(80), 100), ReplacePlan::NoSpace);
        assert_eq!(replace_plan(0, Some(100), 100), ReplacePlan::Reclaim);
        assert_eq!(replace_plan(0, Some(98), 100), ReplacePlan::NoSpace);
    }

    #[test]
    fn replace_config_writes_last_good_and_checks_candidates() {
        make_plugin("plain", "old", false);
        let active = active_config(paths(), "plain").expect("active");
        assert_eq!(std::fs::read(&active).expect("read"), b"old");
        replace_config(paths(), "plain", b"new", false).expect("replace");
        assert_eq!(std::fs::read(&active).expect("read"), b"new");
        assert_eq!(
            std::fs::read(plugin_dir(paths(), "plain").join("config.last-good"))
                .expect("last-good"),
            b"old"
        );
    }

    #[test]
    fn config_check_rejects_a_bad_candidate_and_keeps_the_active_one() {
        make_plugin("checked", "old", true);
        replace_config(paths(), "checked", b"good config", true).expect("good");
        let error = replace_config(paths(), "checked", b"bad config", true).expect_err("bad");
        assert!(error.detail().contains("configCheck"), "{error}");
        assert_eq!(error.problem, Problem::Conflict);
        assert_eq!(
            std::fs::read(active_config(paths(), "checked").expect("active")).expect("read"),
            b"good config"
        );
    }

    /// The daemon's keep-old fallback reads this signal, never the detail text.
    #[test]
    fn an_install_failure_keeps_its_retry_signal_and_its_category() {
        let refused: InstallFailure = Problem::Conflict.error("reserved").into();
        assert!(matches!(refused, InstallFailure::Refused(_)));
        assert_eq!(refused.into_problem().problem, Problem::Conflict);

        let full = InstallFailure::OutOfSpace(Problem::Internal.error("not enough space"));
        assert!(matches!(full, InstallFailure::OutOfSpace(_)));
        assert_eq!(full.into_problem().problem, Problem::Internal);
    }

    #[test]
    fn a_config_refusal_carries_its_status_family() {
        make_plugin("families", "old", false);

        let error =
            replace_config(paths(), "no-such-plugin", b"x", false).expect_err("unknown plugin");
        assert_eq!(error.problem, Problem::UnknownPlugin);

        let oversized = vec![b'x'; 1024 * 1024 + 1];
        let error = replace_config(paths(), "families", &oversized, false).expect_err("too large");
        assert_eq!(error.problem, Problem::TooLarge);
    }

    fn script_manifest(
        id: &str,
        exec: &str,
        data: &[u8],
        interpreter: Option<serde_json::Value>,
        lifecycle: Option<serde_json::Value>,
    ) -> (std::path::PathBuf, package::Manifest) {
        let version_dir = root().join("data/plugins").join(id).join("1.0.0");
        let exec_path = version_dir.join(exec);
        std::fs::create_dir_all(exec_path.parent().expect("parent")).expect("script dir");
        std::fs::write(&exec_path, data).expect("script");
        let mut value = serde_json::json!({
            "schema": 2,
            "name": id,
            "version": "1.0.0",
            "desc": "script test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": exec, "ready": "exec"},
            "files": []
        });
        if let Some(interpreter) = interpreter {
            value["interpreter"] = interpreter;
        }
        if let Some(lifecycle) = lifecycle {
            value["lifecycle"] = lifecycle;
        }
        let bytes = serde_json::to_vec(&value).expect("manifest");
        let manifest = package::parse_manifest(&bytes).expect("parse");
        (version_dir, manifest)
    }

    #[test]
    fn maps_a_schema_two_script_lifecycle_and_interpreter() {
        let host = Interpreter {
            path: "/bin/sh".into(),
            args: Vec::new(),
        };
        let (version_dir, manifest) = script_manifest(
            "scripted",
            "payload/actions.sh",
            b"#!/bin/sh\n",
            Some(serde_json::json!({"name": "sh"})),
            Some(serde_json::json!({
                "start": {"path": "payload/actions.sh", "args": ["start"]},
                "stop": {"path": "payload/actions.sh", "args": ["stop"]},
                "status": {"path": "payload/actions.sh", "args": ["status"]}
            })),
        );

        let service = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "scripted",
            &version_dir,
            &manifest,
            true,
            Some(&host),
        )
        .expect("service");

        let exec = service.exec.as_ref().expect("exec");
        assert_eq!(exec.ready, Ready::Exec);
        assert_eq!(
            exec.interpreter.as_ref().map(|i| i.path.as_str()),
            Some("/bin/sh")
        );
        let lifecycle = service.lifecycle.as_ref().expect("lifecycle");
        let start = lifecycle.start.as_ref().expect("start");
        assert_eq!(start.args, ["start".to_string()]);
        assert_eq!(start.timeout_ms, 30_000);
        assert_eq!(
            start.interpreter.as_ref().map(|i| i.path.as_str()),
            Some("/bin/sh")
        );
        assert!(std::path::Path::new(&start.path).is_absolute());
        assert_eq!(
            std::path::Path::new(&start.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("actions.sh")
        );
        assert_eq!(lifecycle.stop.as_ref().expect("stop").timeout_ms, 10_000);
        assert_eq!(lifecycle.status.as_ref().expect("status").timeout_ms, 5_000);
    }

    #[test]
    fn a_schema_two_script_requires_an_interpreter() {
        let (version_dir, manifest) = script_manifest(
            "no-interpreter",
            "payload/actions.sh",
            b"#!/bin/sh\n",
            None,
            None,
        );

        let error = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "no-interpreter",
            &version_dir,
            &manifest,
            true,
            None,
        )
        .expect_err("missing interpreter");

        assert!(
            error.detail().contains("declares no interpreter"),
            "{error}"
        );
    }

    #[test]
    fn a_native_schema_two_payload_runs_directly() {
        let (version_dir, manifest) =
            script_manifest("native", "payload/bin/tool", b"\x7fELFdemo", None, None);

        let service = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "native",
            &version_dir,
            &manifest,
            true,
            None,
        )
        .expect("service");

        assert!(service.exec.as_ref().expect("exec").interpreter.is_none());
        assert!(service.lifecycle.is_none());
    }

    /// An in-memory package with one payload file, enough for the layout
    /// strategies to run without a daemon.
    fn layout_package(name: &str, version: &str) -> Package {
        let manifest_json = serde_json::json!({
            "schema": 1,
            "name": name,
            "version": version,
            "desc": "layout test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": [{
                "path": "payload/bin/demo",
                "size": 5,
                "sha256": package::hex_sha256(b"hello"),
                "mode": 493
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest_json).expect("manifest json");
        let manifest = package::parse_manifest(&manifest_bytes).expect("manifest");
        Package {
            manifest,
            manifest_bytes,
            signature_bytes: Vec::new(),
            files: vec![package::FileData {
                path: "payload/bin/demo".into(),
                data: b"hello".to_vec(),
                mode: 0o755,
            }],
        }
    }

    #[test]
    fn keep_old_keeps_both_versions_and_replace_drops_the_old_one() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-plugin-layout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let paths = Paths::under(&root);

        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.0.0"),
            Strategy::KeepOld,
            false,
            None,
        )
        .expect("first install");
        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.1.0"),
            Strategy::KeepOld,
            false,
            None,
        )
        .expect("keep-old install");

        let directory = plugin_dir(&paths, "layout-demo");
        assert!(directory.join("1.0.0").is_dir());
        assert!(directory.join("1.1.0").is_dir());
        assert_eq!(
            current_version(&paths, "layout-demo").as_deref(),
            Some("1.1.0")
        );
        assert!(directory.join("current").is_file());

        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.1.0"),
            Strategy::Replace,
            false,
            None,
        )
        .expect("replace install");
        assert!(
            !directory.join("1.0.0").exists(),
            "replace keeps only the new version"
        );
        assert!(directory.join("1.1.0").is_dir());
        assert_eq!(
            current_version(&paths, "layout-demo").as_deref(),
            Some("1.1.0")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dangling_interpreter_on_a_native_payload_is_rejected() {
        let host = Interpreter {
            path: "/bin/sh".into(),
            args: Vec::new(),
        };
        let (version_dir, manifest) = script_manifest(
            "dangling",
            "payload/bin/tool",
            b"\x7fELFdemo",
            Some(serde_json::json!({"name": "sh"})),
            None,
        );

        let error = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "dangling",
            &version_dir,
            &manifest,
            true,
            Some(&host),
        )
        .expect_err("no script payload");

        assert!(error.detail().contains("no script payload"), "{error}");
    }
}
