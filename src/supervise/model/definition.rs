use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::kernel::registry::{
    Exec, Hook, Kind, Lifecycle, Ready, Registry, Restart, Service, State, RESERVED_ID,
};
use crate::manage::valid_name;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadIssue {
    /// File name inside `services.d`; `None` means the directory itself failed.
    pub file: Option<String>,
    /// Best-effort service id, used to explain a rejected `svc status <id>`.
    pub id: String,
    pub reason: String,
}

impl fmt::Display for LoadIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "services.d/{}: {}", file, self.reason),
            None => write!(f, "services.d: {}", self.reason),
        }
    }
}

pub struct Loaded {
    pub registry: Registry,
    pub issues: Vec<LoadIssue>,
}

/// Load the built-in `svc` plus `services.d/*.json`. A rejected definition is
/// skipped with a readable reason; one bad file does not hide the others.
///
/// The root-owned, 0640 layout is a deployment invariant enforced by the OS
/// package manager; this loader assumes the configuration directory is trusted.
pub fn load_services(services_dir: &Path, limits: &crate::platform::limits::Limits) -> Loaded {
    let mut registry = Registry::with_builtin(limits);
    let mut issues = Vec::new();
    let mut sources: BTreeMap<String, Option<String>> = BTreeMap::new();

    let entries = match fs::read_dir(services_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Loaded { registry, issues };
        }
        Err(error) => {
            issues.push(LoadIssue {
                file: None,
                id: String::new(),
                reason: format!("cannot read services.d: {}", error),
            });
            return Loaded { registry, issues };
        }
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("json")
        })
        .collect();
    files.sort();

    for path in files {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        match load_one(&path, limits) {
            Ok(mut service) => {
                if services_dir
                    .join(format!("{}.disabled", service.id))
                    .exists()
                {
                    service.enabled = false;
                }
                let id = service.id.clone();
                match registry.insert(service) {
                    Ok(()) => {
                        sources.insert(id, file);
                    }
                    Err(reason) => issues.push(LoadIssue { file, id, reason }),
                }
            }
            Err(issue) => issues.push(issue),
        }
    }

    load_scripts(services_dir, &mut registry, &mut issues, &sources, limits);

    // A cycle is rejected at load, as the spec requires, but only the services
    // that participate in it are dropped. Removing each cycle terminates.
    while let Some(cycle) = registry.find_cycle() {
        if cycle.iter().any(|id| id == RESERVED_ID) {
            break;
        }
        let path = format!("{} -> {}", cycle.join(" -> "), cycle[0]);
        for id in cycle {
            registry.remove(&id);
            issues.push(LoadIssue {
                file: sources.get(&id).cloned().flatten(),
                id,
                reason: format!("dependency cycle: {}", path),
            });
        }
    }

    Loaded { registry, issues }
}

/// Drop-in scripts live next to the JSON definitions: `<id>.sh` on Unix;
/// `<id>.bat`, `<id>.cmd` or `<id>.ps1` on Windows. The script receives the
/// action as its first argument and is treated as a supervised core service.
fn load_scripts(
    dir: &Path,
    registry: &mut Registry,
    issues: &mut Vec<LoadIssue>,
    sources: &BTreeMap<String, Option<String>>,
    limits: &crate::platform::limits::Limits,
) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            issues.push(LoadIssue {
                file: None,
                id: String::new(),
                reason: format!("cannot read scripts: {}", error),
            });
            return;
        }
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| {
                    crate::supervise::ops::script::extension_supported(
                        &extension.to_ascii_lowercase(),
                    )
                })
                .unwrap_or(false)
        })
        .collect();
    files.sort();

    for path in files {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        let fail = |id: &str, reason: String| LoadIssue {
            file: file.clone(),
            id: id.to_string(),
            reason,
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                issues.push(fail("", format!("cannot read script: {}", error)));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            issues.push(fail(&stem, "script must not be a symlink".into()));
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let id = match path.file_stem().and_then(|stem| stem.to_str()) {
            Some(stem) => stem.to_string(),
            None => continue,
        };
        if id == RESERVED_ID {
            issues.push(fail(&id, format!("id \"{}\" is reserved", RESERVED_ID)));
            continue;
        }
        if !valid_name(&id) {
            issues.push(fail(
                &id,
                format!(
                    "id \"{}\" is invalid; expected [a-z0-9][a-z0-9_-]{{0,63}}",
                    id
                ),
            ));
            continue;
        }
        if let Some(source) = sources.get(&id).and_then(|file| file.clone()) {
            issues.push(fail(
                &id,
                format!("id already defined by services.d/{}", source),
            ));
            continue;
        }
        if registry.get(&id).is_some() {
            issues.push(fail(&id, "id already defined by another service".into()));
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                issues.push(fail(&id, "script must be executable".into()));
                continue;
            }
        }

        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) => {
                issues.push(fail(&id, format!("cannot read script: {}", error)));
                continue;
            }
        };
        let declarations = crate::supervise::ops::script::declared_actions(
            &String::from_utf8_lossy(&contents),
            &extension,
        );
        let restart = crate::supervise::ops::script::declared_restart(
            &String::from_utf8_lossy(&contents),
            &extension,
        );
        // Shell convention: anything that is not `RESTART=always` treats a
        // clean exit as "done" and keeps reporting running (RemainAfterExit).
        let remain_after_exit = restart != Restart::Always;

        let path_text = path.to_string_lossy().to_string();
        let hook = |action: &str, timeout_ms: u64| Hook {
            path: path_text.clone(),
            args: vec![action.to_string()],
            env: BTreeMap::new(),
            timeout_ms,
            interpreter: None,
        };
        let lifecycle = if declarations.start || declarations.stop || declarations.status {
            Some(Lifecycle {
                start: declarations.start.then(|| hook("start", 30_000)),
                stop: declarations.stop.then(|| hook("stop", 10_000)),
                status: declarations.status.then(|| hook("status", 5_000)),
            })
        } else {
            None
        };
        let enabled = !dir.join(format!("{}.disabled", id)).exists();
        let contract = crate::kernel::registry::contract_for(
            &id,
            "1.0.0",
            Vec::new(),
            crate::kernel::registry::Carrier::Slot,
            limits,
        );
        let service = Service {
            id: id.clone(),
            kind: Kind::Core,
            version: "1.0.0".into(),
            protocol: 1,
            desc: format!("script service: {}", file.clone().unwrap_or_default()),
            deps: Vec::new(),
            contract,
            impl_ref: None,
            exec: Some(Exec {
                path: path_text.clone(),
                // Only an implemented start is called with the action argument;
                // otherwise svcd executes the script from the top.
                args: if declarations.start {
                    vec!["start".into()]
                } else {
                    Vec::new()
                },
                env: BTreeMap::new(),
                ready: Ready::Exec,
                interpreter: None,
            }),
            lifecycle,
            enabled,
            restart,
            remain_after_exit,
            compat: None,
            state: State::Stopped,
            pid: None,
            since: None,
            ready_since: None,
            next_at: None,
            failures: 0,
            last_exit: None,
            config: None,
        };
        if let Err(reason) = registry.insert(service) {
            issues.push(fail(&id, reason));
        }
    }
}

fn load_one(path: &Path, limits: &crate::platform::limits::Limits) -> Result<Service, LoadIssue> {
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string);
    let stem = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let fail = |id: &str, reason: String| LoadIssue {
        file: file.clone(),
        id: id.to_string(),
        reason,
    };

    let bytes =
        fs::read(path).map_err(|error| fail(&stem, format!("cannot read file: {}", error)))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| fail(&stem, format!("invalid JSON: {}", error)))?;
    let object = value
        .as_object()
        .ok_or_else(|| fail(&stem, "definition must be a JSON object".into()))?;

    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(&stem)
        .to_string();
    const SPK_ONLY: [&str; 5] = ["platform", "files", "hooks", "config", "ui"];
    let forbidden: Vec<&str> = SPK_ONLY
        .iter()
        .copied()
        .filter(|field| object.contains_key(*field))
        .collect();
    if !forbidden.is_empty() {
        return Err(fail(
            &id,
            format!(
                ".spk-only field(s) not allowed in a core definition: {}",
                forbidden.join(", ")
            ),
        ));
    }
    if object.contains_key("capabilities") {
        return Err(fail(
            &id,
            "core services do not declare capabilities; tools/call is plugin-only".into(),
        ));
    }

    let definition: CoreDefinition = serde_json::from_value(value)
        .map_err(|error| fail(&id, format!("invalid definition: {}", error)))?;
    // The JSON id is authoritative; the file name is only the conventional
    // `<id>.json` location. This is what catches two files claiming one id.
    validate(definition, limits).map_err(|reason| fail(&id, reason))
}

#[derive(Deserialize)]
struct CoreDefinition {
    schema: u32,
    id: String,
    version: String,
    desc: String,
    protocol: u32,
    exec: ExecDefinition,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    lifecycle: Option<LifecycleDefinition>,
    /// Crash-restart policy; `enabled` (the `.disabled` marker) stays the boot
    /// flag. Omitted keeps the historical on-failure behaviour.
    #[serde(default)]
    restart: Option<Restart>,
    /// Keep `running` after a clean exit (one-shot service). Core JSON services
    /// must opt in; drop-in shell scripts inherit the shell convention.
    #[serde(default)]
    remain_after_exit: bool,
}

#[derive(Deserialize)]
struct LifecycleDefinition {
    start: Option<HookDefinition>,
    stop: Option<HookDefinition>,
    status: Option<HookDefinition>,
}

#[derive(Deserialize)]
struct HookDefinition {
    path: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Timeout in seconds; omitted uses the action default.
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Deserialize)]
struct ExecDefinition {
    path: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    ready: Option<String>,
}

fn validate(
    definition: CoreDefinition,
    limits: &crate::platform::limits::Limits,
) -> Result<Service, String> {
    if definition.schema != 1 {
        return Err(format!(
            "unsupported schema {} (expected 1)",
            definition.schema
        ));
    }
    if definition.protocol != 1 {
        return Err(format!(
            "unsupported protocol {} (expected 1)",
            definition.protocol
        ));
    }
    if definition.id == RESERVED_ID {
        return Err(format!(
            "id \"{}\" is reserved for the built-in core service",
            RESERVED_ID
        ));
    }
    if !valid_name(&definition.id) {
        return Err(format!(
            "id \"{}\" is invalid; expected [a-z0-9][a-z0-9_-]{{0,63}}",
            definition.id
        ));
    }
    if definition.version.is_empty() {
        return Err("version must not be empty".into());
    }
    if definition.desc.is_empty() {
        return Err("desc must not be empty".into());
    }

    let exec_path = Path::new(&definition.exec.path);
    if !exec_path.is_absolute() {
        return Err(format!(
            "exec.path must be absolute: \"{}\"",
            definition.exec.path
        ));
    }
    #[cfg(windows)]
    {
        let extension = exec_path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        if !matches!(extension.as_deref(), Some("exe" | "cmd" | "bat")) {
            return Err(format!(
                "exec.path must end with .exe, .cmd or .bat on Windows: \"{}\"",
                definition.exec.path
            ));
        }
    }

    let ready = match definition.exec.ready.as_deref() {
        None | Some("notify") => Ready::Notify,
        Some("exec") => Ready::Exec,
        Some(other) => {
            return Err(format!(
                "exec.ready \"{}\" is invalid; expected notify or exec",
                other
            ))
        }
    };
    // `protocol != 1` was rejected above, so `ready: notify` is the only
    // channel peer a core definition can declare. The predicate is shared with
    // the runtime slot so the two notions of "channel peer" cannot drift.
    if definition.remain_after_exit
        && crate::supervise::check::peer::is_channel_peer(ready, definition.protocol)
    {
        return Err(
            "remain_after_exit is incompatible with a channel peer: exec.ready=notify (or omitted, which defaults to notify) and protocol 3 keep callers on the stdio channel, which ends when the process exits; use exec.ready=exec for a one-shot service".into(),
        );
    }

    let lifecycle = match definition.lifecycle {
        Some(lifecycle) => Some(Lifecycle {
            start: lifecycle
                .start
                .map(|hook| {
                    convert_hook(
                        hook,
                        crate::kernel::registry::HOOK_START_TIMEOUT_MS,
                        crate::kernel::registry::HOOK_TIMEOUT_MAX_MS,
                    )
                })
                .transpose()?,
            stop: lifecycle
                .stop
                .map(|hook| {
                    convert_hook(
                        hook,
                        crate::kernel::registry::HOOK_STOP_TIMEOUT_MS,
                        crate::kernel::registry::HOOK_TIMEOUT_MAX_MS,
                    )
                })
                .transpose()?,
            status: lifecycle
                .status
                .map(|hook| {
                    convert_hook(
                        hook,
                        crate::kernel::registry::HOOK_STATUS_TIMEOUT_MS,
                        crate::kernel::registry::HOOK_STATUS_TIMEOUT_MAX_MS,
                    )
                })
                .transpose()?,
        }),
        None => None,
    };

    for dep in &definition.dependencies {
        if !valid_name(dep) {
            return Err(format!("dependency \"{}\" is not a valid service id", dep));
        }
    }

    let contract = crate::kernel::registry::contract_for(
        &definition.id,
        &definition.version,
        Vec::new(),
        crate::kernel::registry::Carrier::Slot,
        limits,
    );
    Ok(Service {
        id: definition.id,
        kind: Kind::Core,
        version: definition.version,
        protocol: definition.protocol,
        desc: definition.desc,
        deps: definition.dependencies,
        contract,
        impl_ref: None,
        exec: Some(Exec {
            path: definition.exec.path,
            args: definition.exec.args,
            env: definition.exec.env,
            ready,
            interpreter: None,
        }),
        lifecycle,
        // A definition declared in services.d is boot-start in v1; a marker-based
        // enable/disable override belongs to the lifecycle work (#29).
        enabled: true,
        restart: definition.restart.unwrap_or_default(),
        remain_after_exit: definition.remain_after_exit,
        compat: None,
        state: State::Stopped,
        pid: None,
        since: None,
        ready_since: None,
        next_at: None,
        failures: 0,
        last_exit: None,
        config: None,
    })
}

fn convert_hook(
    definition: HookDefinition,
    default_timeout_ms: u64,
    max_timeout_ms: u64,
) -> Result<Hook, String> {
    let path = Path::new(&definition.path);
    if !path.is_absolute() {
        return Err(format!(
            "lifecycle path must be absolute: \"{}\"",
            definition.path
        ));
    }
    #[cfg(windows)]
    {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        if !matches!(extension.as_deref(), Some("exe" | "cmd" | "bat" | "ps1")) {
            return Err(format!(
                "lifecycle path must end with .exe, .cmd, .bat or .ps1 on Windows: \"{}\"",
                definition.path
            ));
        }
    }
    let timeout_ms = match definition.timeout {
        Some(seconds) if seconds >= 1 && seconds <= max_timeout_ms / 1000 => seconds * 1000,
        Some(seconds) => {
            return Err(format!(
                "lifecycle timeout must be between 1 and {} seconds, got {}",
                max_timeout_ms / 1000,
                seconds
            ))
        }
        None => default_timeout_ms,
    };
    Ok(Hook {
        path: definition.path,
        args: definition.args,
        env: definition.env,
        timeout_ms,
        interpreter: None,
    })
}

#[cfg(test)]
mod tests;
