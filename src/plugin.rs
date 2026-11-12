//! Plugin installation layout and the `plugin` scan used by the daemon.
//!
//! The package is verified by [`crate::package`] before this module touches the
//! plugin tree. Versions live under `<data>/plugins/<id>/<version>/`; `current`
//! is a version text file and `enabled` is a marker.

use std::fs;
use std::path::{Path, PathBuf};

use crate::package::{self, Package};
use crate::paths;
use crate::registry::{Exec, Kind, Ready, Service, State, RESERVED_ID};

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

pub fn install_package(package: &Package, strategy: Strategy) -> Result<Service, String> {
    let id = package.manifest.name.clone();
    if id == RESERVED_ID {
        return Err("the built-in svc id is reserved".into());
    }
    if paths::services_dir().join(format!("{}.json", id)).exists() {
        return Err(format!("plugin id conflicts with a core service: {}", id));
    }

    let plugin_dir = plugin_dir(&id);
    let was_installed = current_version(&id).is_some();
    fs::create_dir_all(&plugin_dir)
        .map_err(|error| format!("create {}: {}", plugin_dir.display(), error))?;

    if strategy == Strategy::Replace {
        remove_versions(&plugin_dir, None)?;
    }

    let temp = plugin_dir.join(format!(
        "{}.tmp.{}",
        package.manifest.version,
        std::process::id()
    ));
    if temp.exists() {
        fs::remove_dir_all(&temp)
            .map_err(|error| format!("remove {}: {}", temp.display(), error))?;
    }
    package.write_to(&temp)?;

    let version_dir = plugin_dir.join(&package.manifest.version);
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir)
            .map_err(|error| format!("remove {}: {}", version_dir.display(), error))?;
    }
    fs::rename(&temp, &version_dir)
        .map_err(|error| format!("rename {}: {}", version_dir.display(), error))?;

    write_current(&plugin_dir, &package.manifest.version)?;

    let enabled_marker = plugin_dir.join("enabled");
    let enabled = if was_installed {
        enabled_marker.exists()
    } else {
        fs::write(&enabled_marker, b"")
            .map_err(|error| format!("write {}: {}", enabled_marker.display(), error))?;
        true
    };

    copy_config(package, &plugin_dir)?;

    service_from_manifest(&id, &version_dir, &package.manifest, enabled)
}

pub fn scan() -> (Vec<Service>, Vec<String>) {
    let root = paths::plugins_dir();
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
        let version = match current_version(&id) {
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
        match service_from_manifest(&id, &version_dir, &manifest, enabled) {
            Ok(service) => services.push(service),
            Err(error) => warnings.push(format!("{}: {}", id, error)),
        }
    }
    (services, warnings)
}

pub fn uninstall(id: &str, purge: bool) -> Result<(), String> {
    if !package::valid_name(id) || id == RESERVED_ID {
        return Err(format!("unknown plugin: {}", id));
    }
    let directory = plugin_dir(id);
    if !directory.is_dir() {
        return Err(format!("unknown plugin: {}", id));
    }
    if purge {
        fs::remove_dir_all(&directory)
            .map_err(|error| format!("remove {}: {}", directory.display(), error))?;
        return Ok(());
    }

    remove_versions(&directory, Some("config"))?;
    for name in ["current", "enabled"] {
        let path = directory.join(name);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {}", path.display(), error)),
        }
    }
    Ok(())
}

pub fn plugin_dir(id: &str) -> PathBuf {
    paths::plugins_dir().join(id)
}

pub fn current_version(id: &str) -> Option<String> {
    let text = fs::read_to_string(plugin_dir(id).join("current")).ok()?;
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
    id: &str,
    version_dir: &Path,
    manifest: &package::Manifest,
    enabled: bool,
) -> Result<Service, String> {
    if manifest.platform != package::target() {
        return Err(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            package::target()
        ));
    }
    package::check_path(&manifest.exec.path)?;
    let exec_path = version_dir.join(&manifest.exec.path);
    if !exec_path.is_file() {
        return Err(format!("exec is missing: {}", exec_path.display()));
    }
    let ready = match manifest.exec.ready.as_deref() {
        None | Some("notify") => Ready::Notify,
        Some("exec") => Ready::Exec,
        Some(other) => return Err(format!("unknown ready mode: {}", other)),
    };
    let mut caps = Vec::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty()
            || capability.name.starts_with("svc.")
            || caps.contains(&capability.name)
        {
            return Err(format!("invalid capability name: {}", capability.name));
        }
        caps.push(capability.name.clone());
    }
    let config = {
        let path = plugin_dir(id).join("config");
        path.is_file().then(|| path.to_string_lossy().to_string())
    };

    Ok(Service {
        id: id.to_string(),
        kind: Kind::Plugin,
        version: manifest.version.clone(),
        desc: manifest.desc.clone(),
        deps: manifest.dependencies.clone(),
        caps,
        exec: Some(Exec {
            path: exec_path.to_string_lossy().to_string(),
            args: manifest.exec.args.clone(),
            env: manifest.exec.env.clone(),
            ready,
        }),
        enabled,
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

fn read_manifest(version_dir: &Path) -> Result<package::Manifest, String> {
    let bytes = fs::read(version_dir.join("manifest.json"))
        .map_err(|error| format!("read manifest: {}", error))?;
    let manifest = package::parse_manifest(&bytes)?;
    if manifest.platform != package::target() {
        return Err(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            package::target()
        ));
    }
    Ok(manifest)
}

fn write_current(plugin_dir: &Path, version: &str) -> Result<(), String> {
    let path = plugin_dir.join("current");
    fs::write(&path, version).map_err(|error| format!("write {}: {}", path.display(), error))
}

fn copy_config(package: &Package, plugin_dir: &Path) -> Result<(), String> {
    let Some(template) = package.config_template() else {
        return Ok(());
    };
    let target = plugin_dir.join("config");
    if target.exists() {
        return Ok(());
    }
    fs::write(&target, &template.data)
        .map_err(|error| format!("write {}: {}", target.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&target, fs::Permissions::from_mode(0o640));
    }
    Ok(())
}

fn remove_versions(directory: &Path, keep: Option<&str>) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("read {}: {}", directory.display(), error))?;
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
        fs::remove_dir_all(&path)
            .map_err(|error| format!("remove {}: {}", path.display(), error))?;
    }
    Ok(())
}
