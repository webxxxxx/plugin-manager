//! Manifest parsing and field validation: schema, protocol, compat, lifecycle,
//! hooks and the paths they point at.

use super::*;

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ProblemError> {
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|error| Problem::Invalid.error(error.to_string()))?;
    check_schema(&m)?;
    if m.protocol == 0 || m.protocol > MAX_PROTOCOL {
        return Err(Problem::Invalid.error(format!("unsupported protocol {}", m.protocol)));
    }
    if m.protocol == RAW_PROTOCOL {
        if m.exec.ready.as_deref() != Some("exec") {
            return Err(Problem::Invalid.error("protocol 3 requires exec.ready=exec"));
        }
        if !m.capabilities.is_empty() {
            return Err(Problem::Invalid.error("protocol 3 cannot declare JSON-RPC capabilities"));
        }
    }
    if !valid_name(&m.name) {
        return Err(Problem::Invalid.error("invalid service id"));
    }
    if !m.platform.contains('-') {
        return Err(Problem::UnsupportedPlatform.error("platform must be a target triple"));
    }
    check_version(&m.version)?;
    Ok(m)
}

pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .next()
            .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn valid_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn check_version(value: &str) -> Result<(), ProblemError> {
    if !valid_version(value) {
        return Err(Problem::Invalid.error(format!("version is not semver: {}", value)));
    }
    Ok(())
}

/// Validate every cross-reference a manifest makes, including the hook and
/// capability rules the install path enforces separately. The bundler calls
/// this before writing so authoring mistakes fail early.
pub fn validate_manifest(manifest: &Manifest) -> Result<(), ProblemError> {
    check_schema(manifest)?;
    check_ready(manifest)?;
    check_capabilities_with_exec(manifest)?;
    check_compat(manifest)?;
    if manifest.files.is_empty() {
        return Err(Problem::Invalid.error("package has no payload files"));
    }
    check_manifest_paths(manifest)?;
    check_hooks(manifest)?;
    check_lifecycle(manifest)?;
    check_interpreter(manifest)?;
    if let Some(ui) = &manifest.ui {
        if ui.kind == "web" {
            let entry = ui.entry.as_deref().unwrap_or("index.html");
            let path = format!("payload/web/{}", entry);
            check_path(&path)?;
            if !manifest.files.iter().any(|file| file.path == path) {
                return Err(Problem::Invalid.error(format!("ui entry is not in files[]: {}", path)));
            }
        }
    }
    let mut names = BTreeSet::new();
    for capability in &manifest.capabilities {
        if capability.name.is_empty() {
            return Err(Problem::Invalid.error("capability name must not be empty"));
        }
        if capability.name.starts_with("svc.") {
            return Err(Problem::Invalid.error(format!(
                "capability may not use the svc. prefix: {}",
                capability.name
            )));
        }
        if capability.name == "start" {
            return Err(Problem::Invalid
                .error("capability name \"start\" is reserved for the lifecycle tool"));
        }
        if !names.insert(&capability.name) {
            return Err(
                Problem::Invalid.error(format!("duplicate capability: {}", capability.name))
            );
        }
    }
    Ok(())
}

fn check_schema(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.schema == 0 || manifest.schema > MAX_SCHEMA {
        return Err(Problem::Invalid.error(format!(
            "unsupported schema {} (max {})",
            manifest.schema, MAX_SCHEMA
        )));
    }
    if manifest.schema == 1 && (manifest.interpreter.is_some() || manifest.lifecycle.is_some()) {
        return Err(Problem::Invalid.error(
            "lifecycle and interpreter require schema 2; old daemons would silently ignore them",
        ));
    }
    if manifest.schema < 3 && manifest.compat.is_some() {
        return Err(Problem::Invalid.error(
            "compat requires schema 3; old daemons would ignore the device list and install a \
             package that does not run here",
        ));
    }
    Ok(())
}

fn check_compat(manifest: &Manifest) -> Result<(), ProblemError> {
    let kernel_module = manifest
        .files
        .iter()
        .any(|file| file.path.to_ascii_lowercase().ends_with(".ko"));
    match &manifest.compat {
        Some(spec) => {
            if spec.devices.is_empty() {
                return Err(Problem::Invalid.error(
                    "compat.devices must not be empty; omit compat for a universal package",
                ));
            }
            if spec.devices.len() > 64 {
                return Err(Problem::Invalid.error("compat.devices lists more than 64 ids"));
            }
            let mut seen = BTreeSet::new();
            for device in &spec.devices {
                crate::platform::compat::validate_device_id(device).map_err(|error| {
                    Problem::Invalid.error(format!("compat.devices: {}", error))
                })?;
                if !seen.insert(device) {
                    return Err(
                        Problem::Invalid.error(format!("duplicate compat device id: {}", device))
                    );
                }
            }
        }
        None => {
            if kernel_module {
                return Err(Problem::Invalid.error(
                    "the package contains a kernel module (.ko) but declares no compat.devices; \
                     kernel modules only run on the device generation they were built for",
                ));
            }
        }
    }
    if kernel_module && manifest.schema < 3 {
        return Err(
            Problem::Invalid.error("kernel modules require schema 3 so old daemons reject them")
        );
    }
    Ok(())
}

fn check_ready(manifest: &Manifest) -> Result<(), ProblemError> {
    match manifest.exec.ready.as_deref() {
        None | Some("notify") | Some("exec") => Ok(()),
        Some(other) => Err(Problem::Invalid.error(format!(
            "exec.ready {:?} is invalid; expected notify or exec",
            other
        ))),
    }
}

/// A capabilities list requires a protocol channel, which an exec-ready process
/// does not have (stdin is closed).
fn check_capabilities_with_exec(manifest: &Manifest) -> Result<(), ProblemError> {
    if !manifest.capabilities.is_empty() && manifest.exec.ready.as_deref() == Some("exec") {
        return Err(Problem::Invalid.error(
            "exec.ready=exec cannot declare capabilities: the plugin has no protocol channel",
        ));
    }
    Ok(())
}

fn check_lifecycle(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(lifecycle) = &manifest.lifecycle else {
        return Ok(());
    };
    let mut declared = false;
    for (name, hook, max_seconds) in [
        ("start", lifecycle.start.as_ref(), 300u64),
        ("stop", lifecycle.stop.as_ref(), 300u64),
        ("status", lifecycle.status.as_ref(), 30u64),
    ] {
        let Some(hook) = hook else {
            continue;
        };
        declared = true;
        check_path(&hook.path)?;
        if !manifest.files.iter().any(|file| file.path == hook.path) {
            return Err(Problem::Invalid.error(format!(
                "lifecycle.{} is not in files[]: {}",
                name, hook.path
            )));
        }
        if let Some(timeout) = hook.timeout {
            if timeout == 0 || timeout > max_seconds {
                return Err(Problem::Invalid.error(format!(
                    "lifecycle.{}.timeout must be between 1 and {} seconds",
                    name, max_seconds
                )));
            }
        }
        for argument in &hook.args {
            check_argument("lifecycle", name, argument)?;
        }
        for (key, value) in &hook.env {
            check_argument("lifecycle", name, key)?;
            check_argument("lifecycle", name, value)?;
        }
    }
    if !declared {
        return Err(Problem::Invalid.error("lifecycle declares no start, stop or status action"));
    }
    Ok(())
}

fn check_interpreter(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(interpreter) = &manifest.interpreter else {
        return Ok(());
    };
    if !crate::manage::check::interpreter::valid_name(&interpreter.name) {
        return Err(Problem::Invalid.error(format!(
            "interpreter.name {:?} is not a bare name (no paths or spaces)",
            interpreter.name
        )));
    }
    if let Some(version) = &interpreter.version {
        crate::manage::check::interpreter::validate_constraint(version)
            .map_err(|error| Problem::Invalid.error(format!("interpreter.version: {}", error)))?;
    }
    for argument in &interpreter.args {
        check_argument("interpreter", &interpreter.name, argument)?;
    }
    Ok(())
}

fn check_argument(section: &str, name: &str, value: &str) -> Result<(), ProblemError> {
    if value.contains('\0') {
        return Err(Problem::Invalid.error(format!("{} {} contains a NUL byte", section, name)));
    }
    Ok(())
}

/// `true` when the manifest actually names at least one hook; an empty
/// `"hooks": {}` object is not an install-time hook package.
pub fn declares_hooks(manifest: &Manifest) -> bool {
    manifest
        .hooks
        .as_ref()
        .map(|hooks| hooks.post_install.is_some() || hooks.pre_remove.is_some())
        .unwrap_or(false)
}

/// Hooks are package-signed executables under `hooks/`; the manifest path is
/// resolved inside the installed version directory at run time.
pub(crate) fn check_hooks(manifest: &Manifest) -> Result<(), ProblemError> {
    for (name, path) in [
        (
            "postInstall",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.post_install.as_deref()),
        ),
        (
            "preRemove",
            manifest
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.pre_remove.as_deref()),
        ),
    ] {
        let Some(path) = path else { continue };
        check_path(path)?;
        if !path.starts_with("hooks/") {
            return Err(Problem::Invalid.error(format!("{} must be under hooks/: {}", name, path)));
        }
        let file = manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                Problem::Invalid.error(format!("{} is not in files[]: {}", name, path))
            })?;
        check_hook_executable(name, path, file.mode)?;
    }
    Ok(())
}

fn check_hook_executable(name: &str, path: &str, mode: u32) -> Result<(), ProblemError> {
    #[cfg(unix)]
    {
        if mode & 0o111 == 0 {
            return Err(Problem::Invalid.error(format!("{} must be executable: {}", name, path)));
        }
    }
    #[cfg(not(unix))]
    {
        // Windows has no exec bit; the hook runner rejects a file that is not
        // .exe/.cmd/.bat when it actually runs it.
        let _ = (name, path, mode);
    }
    Ok(())
}

pub(crate) fn check_manifest_paths(manifest: &Manifest) -> Result<(), ProblemError> {
    check_path(&manifest.exec.path)?;
    if !manifest
        .files
        .iter()
        .any(|file| file.path == manifest.exec.path)
    {
        return Err(Problem::Invalid.error(format!(
            "exec.path is not in files[]: {}",
            manifest.exec.path
        )));
    }
    if let Some(config) = &manifest.config {
        check_path(config)?;
        if !manifest.files.iter().any(|file| &file.path == config) {
            return Err(Problem::Invalid.error(format!("config is not in files[]: {}", config)));
        }
    }
    if let Some(check) = &manifest.config_check {
        check_path(&check.path)?;
        if !manifest.files.iter().any(|file| file.path == check.path) {
            return Err(Problem::Invalid.error(format!(
                "configCheck.path is not in files[]: {}",
                check.path
            )));
        }
        let config = manifest.config.as_deref().unwrap_or("config/config");
        if !manifest.files.iter().any(|file| file.path == config) {
            return Err(Problem::Invalid.error("configCheck is declared but config is missing"));
        }
    }
    if let Some(ui) = &manifest.ui {
        if let Some(entry) = &ui.entry {
            check_path(entry)?;
        }
    }
    Ok(())
}

/// The host-platform gate. `Inspect` deliberately skips it: a package built
/// for another target is still well-formed, and the caller gets
/// `host.matchesHost` as a fact.
pub(crate) fn check_platform(manifest: &Manifest) -> Result<(), ProblemError> {
    if manifest.platform != crate::platform::target() {
        return Err(Problem::UnsupportedPlatform.error(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            crate::platform::target()
        )));
    }
    Ok(())
}
