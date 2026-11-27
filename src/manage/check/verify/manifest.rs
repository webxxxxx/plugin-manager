//! Manifest parsing and field validation: schema, protocol, compat, lifecycle,
//! hooks and the paths they point at.

use super::*;

use crate::kernel::registry::valid_id;

/// Icon ceilings: one file per format, and the whole icon set stays small
/// enough that it never competes with the payload budget.
pub(crate) const MAX_ICON_BYTES: u64 = 1024 * 1024;
pub(crate) const MAX_ICONS_TOTAL_BYTES: u64 = 2 * 1024 * 1024;

pub fn parse_manifest(bytes: &[u8]) -> Result<Manifest, ProblemError> {
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|error| Problem::Invalid.error(error.to_string()))?;
    check_schema(&m)?;
    check_identity(&m)?;
    if m.protocol == 0 || m.protocol > MAX_PROTOCOL {
        return Err(Problem::Invalid.error(format!("unsupported protocol {}", m.protocol)));
    }
    if !m.platform.contains('-') {
        return Err(Problem::UnsupportedPlatform.error("platform must be a target triple"));
    }
    check_version(&m.version)?;
    Ok(m)
}

/// `id` is the plugin identity and `name` is only a display name. A package
/// without `id` predates schema 1 or was hand-assembled; either way the fix is
/// the bundler, so the error says so instead of guessing an id.
fn check_identity(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(id) = manifest.id.as_deref() else {
        return Err(Problem::Invalid
            .error("schema 1 requires an id; repack with `plugin bundle --id <id>`"));
    };
    if !valid_id(id) {
        return Err(Problem::Invalid.error(format!(
            "invalid plugin id: {} (expected [a-z0-9][a-z0-9_-]{{0,63}})",
            id
        )));
    }
    if id == crate::kernel::registry::RESERVED_ID {
        return Err(Problem::Invalid
            .error("the built-in svc id is reserved and cannot be used as a plugin id"));
    }
    if !valid_display_name(&manifest.name) {
        return Err(Problem::Invalid.error(
            "name is the display name: it must be non-empty, at most 128 characters and free of \
             control characters",
        ));
    }
    Ok(())
}

/// The display name may be any UTF-8 (Chinese included); only emptiness, the
/// length ceiling and control characters are rejected.
pub fn valid_display_name(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= 128 && !s.chars().any(char::is_control)
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
    check_icons(manifest)?;
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
    if manifest.schema != MAX_SCHEMA {
        return Err(Problem::Invalid.error(format!(
            "unsupported manifest schema {}: this daemon reads schema {} only (packages written \
             for the old 1/2/3 line also lack the required `id`); repack with `plugin bundle`",
            manifest.schema, MAX_SCHEMA
        )));
    }
    Ok(())
}

/// Package icons: `icons` names one payload file per format. The size ceilings
/// keep a package from smuggling a 100 MiB bitmap through the icon field; the
/// bytes themselves are magic-checked where the payload is available.
fn check_icons(manifest: &Manifest) -> Result<(), ProblemError> {
    let Some(icons) = &manifest.icons else {
        return Ok(());
    };
    if icons.entries().next().is_none() {
        return Err(Problem::Invalid.error("icons must declare at least one of png, ico or icns"));
    }
    let mut total = 0u64;
    for (format, path) in icons.entries() {
        check_path(path)?;
        let entry = manifest
            .files
            .iter()
            .find(|file| file.path == path)
            .ok_or_else(|| {
                Problem::Invalid.error(format!("icons.{} is not in files[]: {}", format, path))
            })?;
        if entry.size > MAX_ICON_BYTES {
            return Err(Problem::TooLarge.error(format!(
                "icons.{} is {} bytes; the per-icon ceiling is {}",
                format, entry.size, MAX_ICON_BYTES
            )));
        }
        total += entry.size;
    }
    if total > MAX_ICONS_TOTAL_BYTES {
        return Err(Problem::TooLarge.error(format!(
            "icons total {} bytes; the ceiling is {}",
            total, MAX_ICONS_TOTAL_BYTES
        )));
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
    Ok(())
}

fn check_ready(manifest: &Manifest) -> Result<(), ProblemError> {
    crate::kernel::registry::ready_from(
        manifest.exec.ready.as_deref(),
        manifest.exec.port,
        manifest.exec.probe_addr.as_deref(),
    )
    .map_err(|error| Problem::Invalid.error(error.to_string()))?;
    Ok(())
}

/// One capability has exactly one way in: the protocol channel for a
/// `notify`-ready plugin, a one-shot hook for a plugin with no channel (#342).
fn check_capabilities_with_exec(manifest: &Manifest) -> Result<(), ProblemError> {
    let no_channel = matches!(manifest.exec.ready.as_deref(), Some("exec") | Some("port"));
    if manifest.capabilities.is_empty() {
        return Ok(());
    }
    if !no_channel {
        if manifest
            .capabilities
            .iter()
            .any(|capability| capability.hook.is_some())
        {
            return Err(Problem::Invalid.error(
                "a capability hook is only for a plugin with no protocol channel; \
                 exec.ready=notify already delivers the call",
            ));
        }
        return Ok(());
    }
    // No channel: every capability has to bring its own action, or it would be
    // listed as a tool that can never answer.
    for capability in &manifest.capabilities {
        let Some(hook) = capability.hook.as_ref() else {
            return Err(Problem::Invalid.error(format!(
                "exec.ready=exec/port has no protocol channel, so capability {} needs a hook",
                capability.name
            )));
        };
        check_path(&hook.path)?;
        if !manifest.files.iter().any(|file| file.path == hook.path) {
            return Err(Problem::Invalid.error(format!(
                "capabilities[{}].hook is not in files[]: {}",
                capability.name, hook.path
            )));
        }
        if let Some(timeout) = hook.timeout {
            if timeout == 0 || timeout > 300 {
                return Err(Problem::Invalid.error(format!(
                    "capabilities[{}].hook.timeout must be between 1 and 300 seconds",
                    capability.name
                )));
            }
        }
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
        ("started", lifecycle.started.as_ref(), 300u64),
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
        return Err(
            Problem::Invalid.error("lifecycle declares no start, started, stop or status action")
        );
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
