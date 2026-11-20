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

mod keep_old;
mod replace;
mod space;

use keep_old::{install_keep_old, install_keep_old_staged};
use replace::{install_replace, install_replace_staged};
use space::{directory_size, free_space, out_of_space};

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

pub fn root_required() -> Result<(), ProblemError> {
    if crate::platform::is_privileged() || crate::platform::paths::user_mode() {
        Ok(())
    } else {
        Err(Problem::Internal.error("root / Administrator is required"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manage::check::plugin::Strategy;
    use crate::manage::model::layout::{current_version, plugin_dir};
    use crate::manage::model::manifest::Package;
    use crate::platform::paths::Paths;

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
            &mut || {},
        )
        .expect("first install");
        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.1.0"),
            Strategy::KeepOld,
            false,
            None,
            &mut || {},
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
            &mut || {},
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
}
