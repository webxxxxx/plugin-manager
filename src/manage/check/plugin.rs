//! 安装前的纯判定：策略、空间替换计划、hook 准入与包校验。
use std::path::Path;

use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::registry::Interpreter;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage::check::interpreter;
use crate::manage::model::manifest::Package;
use crate::manage::ops::scan::resolve_interpreter;
use crate::manage::package;
use crate::platform::compat;
use crate::platform::paths::Paths;

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
pub(crate) enum ReplacePlan {
    Fresh,
    Backup,
    Reclaim,
    NoSpace,
}

pub(crate) fn replace_plan(free: u64, old: Option<u64>, new: u64) -> ReplacePlan {
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
pub(crate) fn hooks_allowed(opt_in_hooks: bool) -> bool {
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
