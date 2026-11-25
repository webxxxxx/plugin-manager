//! 扫描已安装插件：读清单、投影成 `Service`、解析解释器与平台文件。

use std::fs;
use std::path::Path;

use crate::kernel::contract::{Capability as KernelCapability, CapabilityName, Effect};
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::registry::{
    Exec, Hook, Interpreter, Kind, Lifecycle, Ready, Restart, Service, State, RESERVED_ID,
};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage as package;
use crate::manage::check::interpreter;
use crate::platform::paths::Paths;

pub fn scan(
    paths: &Paths,
    limits: &crate::platform::limits::Limits,
) -> (Vec<Service>, Vec<String>) {
    let host = crate::platform::compat::detect(paths, MAX_SCHEMA, MAX_PROTOCOL);
    let mut services = Vec::new();
    let mut warnings = Vec::new();
    // The roots are the truth and the location index is a hint, so one walk
    // does both: it finds every installed plugin (any root) and leaves the
    // index agreeing with the disk (ADR-0030). Entries whose root is not
    // available right now are kept: a pulled card must not uninstall its
    // plugins by accident.
    let discovery = crate::platform::locations::discover(&paths.plugin_roots);
    warnings.extend(discovery.warnings.iter().cloned());
    warnings.extend(discovery.conflicts.iter().cloned());
    let recorded = crate::platform::locations::load(paths);
    let found = crate::platform::locations::rebuild(&discovery, recorded.clone());
    if recorded != found {
        if let Err(error) = crate::platform::locations::save(paths, &found) {
            warnings.push(format!("plugin locations: {}", error));
        }
    }

    for entry in &discovery.found {
        let id = entry.id.as_str();
        if !crate::kernel::registry::valid_id(id) || id == RESERVED_ID {
            warnings.push(format!("ignoring plugin directory {}", id));
            continue;
        }
        let directory = entry.root.join(id);
        let version = match crate::manage::model::layout::current_version_at(&directory) {
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
            limits,
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
                match crate::platform::compat::evaluate(crate::platform::target(), &devices, &host)
                {
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
    limits: &crate::platform::limits::Limits,
    version_dir: &Path,
    manifest: &package::Manifest,
    enabled: bool,
    interpreter: Option<&Interpreter>,
) -> Result<Service, ProblemError> {
    let id = manifest.id();
    if manifest.platform != crate::platform::target() {
        return Err(Problem::UnsupportedPlatform.error(format!(
            "platform mismatch: package {}, host {}",
            manifest.platform,
            crate::platform::target()
        )));
    }
    crate::kernel::layout::check_path(&manifest.exec.path)?;
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
            output_schema: capability.output_schema.clone(),
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

    let exec_is_script = !is_native_file(&exec_path)?;
    let mut any_script = exec_is_script;

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
    if !any_script && interpreter.is_some() {
        return Err(Problem::Invalid.error(format!(
            "plugin {} declares an interpreter but has no script payload",
            id
        )));
    }

    let config = {
        // The tree the manifest was read from, not what the location index
        // says: an install builds this service *before* it records its root, so
        // asking the index at that moment answered the default root and a
        // non-default install came out without a config path — no
        // `SVCD_CONFIG`, and the console hid the 配置 button (#307).
        let directory = version_dir.parent().unwrap_or(version_dir);
        let path = directory.join("config");
        path.is_file().then(|| path.to_string_lossy().to_string())
    };

    Ok(Service {
        id: id.to_string(),
        kind: Kind::Plugin,
        version: manifest.version.clone(),
        protocol: manifest.protocol,
        desc: manifest.desc.clone(),
        display_name: Some(manifest.name.clone()),
        deps: manifest.dependencies.clone(),
        contract,
        impl_ref: None,
        exec: Some(Exec {
            path: exec_path.to_string_lossy().to_string(),
            args: manifest.exec.args.clone(),
            env: manifest.exec.env.clone(),
            ready,
            interpreter: if exec_is_script {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::contract::Effect;
    use crate::kernel::registry::{Interpreter, Ready};
    use crate::manage::test_support::*;

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
            "schema": 1,
            "id": id,
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

    /// #307: an install to a second root builds the service before the root is
    /// recorded, so the config path must come from the version directory it was
    /// reading, not from the index (which still answers the default root).
    #[test]
    fn the_config_path_follows_the_version_directory() {
        let temp = crate::platform::test_support::TempDir::with_prefix("svcd-scan");
        let card = temp.path().join("card");
        let version_dir = card.join("demo").join("1.0.0");
        std::fs::create_dir_all(version_dir.join("payload")).expect("payload");
        std::fs::write(version_dir.join("payload/run.sh"), b"#!/bin/sh\n").expect("script");
        std::fs::write(card.join("demo").join("config"), b"{\"a\":1}\n").expect("config");
        let value = serde_json::json!({
            "schema": 1,
            "id": "demo",
            "name": "demo",
            "version": "1.0.0",
            "desc": "config test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/run.sh", "ready": "exec"},
            "interpreter": {"name": "sh"},
            "files": []
        });
        let manifest =
            package::parse_manifest(&serde_json::to_vec(&value).expect("manifest")).expect("parse");
        // The card root is configured but not default, and the index has no
        // entry for `demo` yet — exactly the state a fresh install is in.
        let mut paths = Paths::under(temp.path());
        paths.plugin_roots = vec![temp.path().join("data").join("plugins"), card.clone()];
        let interpreter = Interpreter {
            path: "/bin/sh".into(),
            args: Vec::new(),
        };

        let service = service_from_manifest(
            &crate::platform::limits::test_default(),
            &version_dir,
            &manifest,
            true,
            Some(&interpreter),
        )
        .expect("service");

        assert_eq!(
            service.config.as_deref(),
            Some(card.join("demo").join("config").to_string_lossy().as_ref()),
            "the config must be the one next to the installed version"
        );
    }

    #[test]
    fn maps_a_script_lifecycle_and_interpreter() {
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
            &crate::platform::limits::test_default(),
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
    fn a_script_without_an_interpreter_runs_directly() {
        let (version_dir, manifest) = script_manifest(
            "no-interpreter",
            "payload/actions.sh",
            b"#!/bin/sh\n",
            None,
            None,
        );

        let service = service_from_manifest(
            &crate::platform::limits::test_default(),
            &version_dir,
            &manifest,
            true,
            None,
        )
        .expect("direct exec");

        assert!(service.exec.as_ref().expect("exec").interpreter.is_none());
    }

    #[test]
    fn a_native_payload_runs_directly() {
        let (version_dir, manifest) =
            script_manifest("native", "payload/bin/tool", b"\x7fELFdemo", None, None);

        let service = service_from_manifest(
            &crate::platform::limits::test_default(),
            &version_dir,
            &manifest,
            true,
            None,
        )
        .expect("service");

        assert!(service.exec.as_ref().expect("exec").interpreter.is_none());
        assert!(service.lifecycle.is_none());
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
            &crate::platform::limits::test_default(),
            &version_dir,
            &manifest,
            true,
            Some(&host),
        )
        .expect_err("no script payload");

        assert!(error.detail().contains("no script payload"), "{error}");
    }
}
