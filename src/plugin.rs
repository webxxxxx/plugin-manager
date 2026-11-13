//! Plugin installation layout and the `plugin` scan used by the daemon.
//!
//! The package is verified by [`crate::package`] before this module touches the
//! plugin tree. Versions live under `<data>/plugins/<id>/<version>/`; `current`
//! is a version text file and `enabled` is a marker.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::log;
use crate::package::{self, Package};
use crate::paths;
use crate::registry::{Capability, Exec, Kind, Ready, Service, State, RESERVED_ID};

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

    match strategy {
        Strategy::KeepOld => install_keep_old(package, &id, &plugin_dir, was_installed),
        Strategy::Replace => install_replace(package, &id, &plugin_dir, was_installed),
    }
}

fn install_keep_old(
    package: &Package,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
) -> Result<Service, String> {
    let (version_dir, enabled) = write_version(package, plugin_dir, was_installed)?;
    copy_config(package, plugin_dir)?;
    service_from_manifest(id, &version_dir, &package.manifest, enabled)
}

fn install_replace(
    package: &Package,
    id: &str,
    plugin_dir: &Path,
    was_installed: bool,
) -> Result<Service, String> {
    let old_version = current_version(id).filter(|version| plugin_dir.join(version).is_dir());
    let old_dir = old_version.as_ref().map(|version| plugin_dir.join(version));
    let old_size = match &old_dir {
        Some(path) => Some(directory_size(path)?),
        None => None,
    };
    let new_size = package.unpacked_size();
    let free = free_space(plugin_dir)?;
    let plan = replace_plan(free, old_size, new_size);
    if plan == ReplacePlan::NoSpace {
        return Err(format!(
            "not enough space to install {} {}: need {} bytes, {} free{}",
            id,
            package.manifest.version,
            new_size,
            free,
            old_size
                .map(|size| format!(", current version uses {size} bytes"))
                .unwrap_or_default()
        ));
    }

    let backup = if plan == ReplacePlan::Backup {
        let old_dir = old_dir.as_ref().expect("backup implies an old version");
        let old_version = old_version.as_ref().expect("backup implies an old version");
        let backup = plugin_dir.join(format!(".backup-{}.{}", old_version, std::process::id()));
        if backup.exists() {
            fs::remove_dir_all(&backup)
                .map_err(|error| format!("remove {}: {}", backup.display(), error))?;
        }
        fs::rename(old_dir, &backup)
            .map_err(|error| format!("backup {}: {}", old_dir.display(), error))?;
        Some(backup)
    } else {
        if plan == ReplacePlan::Reclaim {
            if let Some(old_dir) = &old_dir {
                fs::remove_dir_all(old_dir)
                    .map_err(|error| format!("remove {}: {}", old_dir.display(), error))?;
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
            service_from_manifest(id, &version_dir, &package.manifest, enabled)
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
            let _ = fs::remove_dir_all(plugin_dir.join(&package.manifest.version));
            if !enabled_existed {
                let _ = fs::remove_file(plugin_dir.join("enabled"));
            }
            snapshot.restore(plugin_dir);
            if let (Some(backup), Some(old_dir), Some(old_version)) =
                (&backup, &old_dir, &old_version)
            {
                if let Err(restore) = fs::rename(backup, old_dir) {
                    return Err(format!(
                        "{} (rollback failed: cannot restore {}: {})",
                        error,
                        old_dir.display(),
                        restore
                    ));
                }
                if let Err(restore) = write_current(plugin_dir, old_version) {
                    return Err(format!("{} (rollback failed: {})", error, restore));
                }
            } else if backup.is_none() {
                let _ = fs::remove_file(plugin_dir.join("current"));
            }
            Err(error)
        }
    }
}

fn write_version(
    package: &Package,
    plugin_dir: &Path,
    was_installed: bool,
) -> Result<(PathBuf, bool), String> {
    let temp = plugin_dir.join(format!(
        "{}.tmp.{}",
        package.manifest.version,
        std::process::id()
    ));
    if temp.exists() {
        fs::remove_dir_all(&temp)
            .map_err(|error| format!("remove {}: {}", temp.display(), error))?;
    }
    if let Err(error) = package.write_to(&temp) {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }

    let version_dir = plugin_dir.join(&package.manifest.version);
    if version_dir.exists() {
        fs::remove_dir_all(&version_dir)
            .map_err(|error| format!("remove {}: {}", version_dir.display(), error))?;
    }
    fs::rename(&temp, &version_dir)
        .map_err(|error| format!("rename {}: {}", version_dir.display(), error))?;

    write_current(plugin_dir, &package.manifest.version)?;

    let enabled_marker = plugin_dir.join("enabled");
    let enabled = if was_installed {
        enabled_marker.exists()
    } else {
        fs::write(&enabled_marker, b"")
            .map_err(|error| format!("write {}: {}", enabled_marker.display(), error))?;
        true
    };
    Ok((version_dir, enabled))
}

fn directory_size(path: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    let entries =
        fs::read_dir(path).map_err(|error| format!("read {}: {}", path.display(), error))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("read {}: {}", path.display(), error))?;
        let metadata = entry
            .metadata()
            .map_err(|error| format!("stat {}: {}", entry.path().display(), error))?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn free_space(path: &Path) -> Result<u64, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| format!("free space: invalid path {}", path.display()))?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
            return Err(format!(
                "free space {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
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
            return Err(format!(
                "free space {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(available)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err("free space is not supported on this platform".into())
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

    fn restore(&self, plugin_dir: &Path) {
        restore_file(&plugin_dir.join("config"), &self.config, true);
        restore_file(
            &plugin_dir.join("config.template.sha256"),
            &self.template_hash,
            false,
        );
        restore_file(&plugin_dir.join("config.new"), &self.config_new, false);
    }
}

fn restore_file(path: &Path, content: &Option<Vec<u8>>, config: bool) {
    match content {
        Some(bytes) if config => {
            let _ = write_config(path, bytes);
        }
        Some(bytes) => {
            let _ = fs::write(path, bytes);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
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
    let mut caps: Vec<Capability> = Vec::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty()
            || capability.name.starts_with("svc.")
            || capability.name == "start"
            || caps.iter().any(|cap| cap.name == capability.name)
        {
            return Err(format!("invalid capability name: {}", capability.name));
        }
        caps.push(Capability {
            name: capability.name.clone(),
            description: capability.desc.clone(),
            input_schema: capability.input_schema.clone(),
        });
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
        lifecycle: None,
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

fn write_hash(path: &Path, hash: &str) -> Result<(), String> {
    fs::write(path, hash).map_err(|error| format!("write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o640));
    }
    Ok(())
}

pub fn lock_config(id: &str) -> Result<fs::File, String> {
    let path = plugin_dir(id).join("config.lock");
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("open {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(file)
}

pub fn write_config(path: &Path, data: &[u8]) -> Result<(), String> {
    fs::write(path, data).map_err(|error| format!("write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640))
            .map_err(|error| format!("chmod {}: {}", path.display(), error))?;
    }
    Ok(())
}

pub fn active_config(id: &str) -> Option<PathBuf> {
    let path = plugin_dir(id).join("config");
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
pub fn web_assets(id: &str) -> Option<WebAssets> {
    let version = current_version(id)?;
    let version_dir = plugin_dir(id).join(version);
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

pub fn config_template(id: &str) -> Option<(Vec<u8>, package::Manifest)> {
    let version = current_version(id)?;
    let version_dir = plugin_dir(id).join(version);
    let manifest = read_manifest(&version_dir).ok()?;
    let config = manifest.config.as_deref().unwrap_or("config/config");
    let data = fs::read(version_dir.join(config)).ok()?;
    Some((data, manifest))
}

pub fn config_check(id: &str, candidate: &Path) -> Result<(), String> {
    let Some(version) = current_version(id) else {
        return Err(format!("unknown plugin: {}", id));
    };
    let version_dir = plugin_dir(id).join(&version);
    let manifest = read_manifest(&version_dir)?;
    let Some(check) = manifest.config_check else {
        return Ok(());
    };
    package::check_path(&check.path)?;
    let program = version_dir.join(&check.path);
    if !program.is_file() {
        return Err(format!("configCheck is missing: {}", program.display()));
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
        .env("SVCD_DATA_DIR", paths::data_dir())
        .env("SVCD_CONFIG_DIR", paths::config_dir())
        .env("SVCD_CONFIG", candidate)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn configCheck: {}", error))?;
    let _ = crate::proc::assign(&child);
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
                return Err(format!(
                    "configCheck failed ({}): {}",
                    status,
                    stderr.trim()
                ));
            }
            Ok(None) => {}
            Err(error) => return Err(format!("configCheck: {}", error)),
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("configCheck timed out after 10s".into());
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

pub fn replace_config(id: &str, candidate: &[u8], check: bool) -> Result<(), String> {
    let directory = plugin_dir(id);
    if !directory.is_dir() {
        return Err(format!("unknown plugin: {}", id));
    }
    if candidate.len() > 1024 * 1024 {
        return Err("config exceeds 1 MiB".into());
    }
    let target = directory.join("config");
    let temporary = directory.join(format!("config.tmp.{}", std::process::id()));
    write_config(&temporary, candidate)?;
    if check {
        if let Err(error) = config_check(id, &temporary) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    }
    if target.exists() {
        fs::copy(&target, directory.join("config.last-good"))
            .map_err(|error| format!("save last-good: {}", error))?;
    }
    fs::rename(&temporary, &target).map_err(|error| format!("replace config: {}", error))?;
    Ok(())
}

pub fn root_required() -> Result<(), String> {
    if crate::install::is_privileged() {
        Ok(())
    } else {
        Err("root / Administrator is required".into())
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

pub fn export_config(id: &str) -> Result<(String, Vec<u8>), String> {
    let version = current_version(id).ok_or_else(|| format!("unknown plugin: {}", id))?;
    let config = active_config(id).ok_or_else(|| format!("plugin {} has no config", id))?;
    let data = fs::read(&config).map_err(|error| format!("read config: {}", error))?;
    Ok((version, data))
}

pub fn export_spk(id: &str) -> Result<(String, Vec<u8>), String> {
    let version = current_version(id).ok_or_else(|| format!("unknown plugin: {}", id))?;
    let bytes = package::installed_spk(&plugin_dir(id).join(&version))?;
    Ok((version, bytes))
}

pub fn export_spkb(id: &str) -> Result<(String, Vec<u8>), String> {
    let version = current_version(id).ok_or_else(|| format!("unknown plugin: {}", id))?;
    let spec = export_spk(id)?.1;
    let config = active_config(id)
        .map(|path| fs::read(path).map_err(|error| format!("read config: {}", error)))
        .transpose()?;
    let exported_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let backup = serde_json::json!({
        "schema": 1,
        "id": id,
        "version": version,
        "exportedAt": exported_at,
        "configSha256": config.as_ref().map(|data| package::hex_sha256(data)),
    });
    let backup = serde_json::to_vec(&backup).map_err(|error| error.to_string())?;
    let mut entries: Vec<(&str, &[u8], u32)> = vec![
        ("backup.json", &backup, 0o644),
        ("plugin.spk", &spec, 0o644),
    ];
    if let Some(config) = &config {
        entries.push(("config", config, 0o600));
    }
    Ok((version, package::write_tar(&entries)?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    fn root() -> &'static Path {
        ROOT.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "svcd-config-unit-{}-{}",
                std::process::id(),
                nanos
            ));
            std::env::set_var("SVCD_DATA_DIR", path.join("data"));
            std::env::set_var("SVCD_CONFIG_DIR", path.join("config"));
            std::fs::create_dir_all(path.join("data/plugins")).expect("data");
            path
        })
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
            "platform": package::target(),
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
        let active = active_config("plain").expect("active");
        assert_eq!(std::fs::read(&active).expect("read"), b"old");
        replace_config("plain", b"new", false).expect("replace");
        assert_eq!(std::fs::read(&active).expect("read"), b"new");
        assert_eq!(
            std::fs::read(plugin_dir("plain").join("config.last-good")).expect("last-good"),
            b"old"
        );
    }

    #[test]
    fn config_check_rejects_a_bad_candidate_and_keeps_the_active_one() {
        make_plugin("checked", "old", true);
        replace_config("checked", b"good config", true).expect("good");
        let error = replace_config("checked", b"bad config", true).expect_err("bad");
        assert!(error.contains("configCheck"), "{error}");
        assert_eq!(
            std::fs::read(active_config("checked").expect("active")).expect("read"),
            b"good config"
        );
    }
}
