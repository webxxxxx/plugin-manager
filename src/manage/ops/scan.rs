//! 扫描已安装插件：读清单、投影成 `Service`、解析解释器与平台文件。

use std::fs;
use std::path::Path;

use crate::kernel::contract::{Capability as KernelCapability, CapabilityName, Effect};
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::registry::{
    Exec, Hook, Interpreter, Kind, Lifecycle, Ready, Restart, Service, State, RESERVED_ID,
};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage::check::interpreter;
use crate::manage::package;
use crate::manage::plugin::{current_version, plugin_dir};
use crate::platform::paths::Paths;

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

pub(crate) fn service_from_manifest(
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

pub(crate) fn convert_lifecycle_hook(
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

pub(crate) fn read_manifest(version_dir: &Path) -> Result<package::Manifest, ProblemError> {
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

pub(crate) fn resolve_interpreter(
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

/// Read only the header: installed payloads can be large native binaries.
pub(crate) fn is_native_file(path: &Path) -> Result<bool, ProblemError> {
    use std::io::Read;
    let mut file = fs::File::open(path)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    let mut head = [0u8; 8];
    let read = file
        .read(&mut head)
        .map_err(|error| Problem::Internal.error(format!("read {}: {}", path.display(), error)))?;
    Ok(package::is_native_binary(&head[..read]))
}
