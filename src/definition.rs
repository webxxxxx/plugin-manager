use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::package::valid_name;
use crate::registry::{Exec, Hook, Kind, Lifecycle, Ready, Registry, Service, State, RESERVED_ID};

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
pub fn load_services(services_dir: &Path) -> Loaded {
    let mut registry = Registry::with_builtin();
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
        match load_one(&path) {
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

    load_scripts(services_dir, &mut registry, &mut issues, &sources);

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
                    crate::script::extension_supported(&extension.to_ascii_lowercase())
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
        let declarations =
            crate::script::declared_actions(&String::from_utf8_lossy(&contents), &extension);

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
        let service = Service {
            id: id.clone(),
            kind: Kind::Core,
            version: "1.0.0".into(),
            desc: format!("script service: {}", file.clone().unwrap_or_default()),
            deps: Vec::new(),
            caps: Vec::new(),
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

fn load_one(path: &Path) -> Result<Service, LoadIssue> {
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
    validate(definition).map_err(|reason| fail(&id, reason))
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

fn validate(definition: CoreDefinition) -> Result<Service, String> {
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

    let lifecycle = match definition.lifecycle {
        Some(lifecycle) => Some(Lifecycle {
            start: lifecycle
                .start
                .map(|hook| {
                    convert_hook(
                        hook,
                        crate::registry::HOOK_START_TIMEOUT_MS,
                        crate::registry::HOOK_TIMEOUT_MAX_MS,
                    )
                })
                .transpose()?,
            stop: lifecycle
                .stop
                .map(|hook| {
                    convert_hook(
                        hook,
                        crate::registry::HOOK_STOP_TIMEOUT_MS,
                        crate::registry::HOOK_TIMEOUT_MAX_MS,
                    )
                })
                .transpose()?,
            status: lifecycle
                .status
                .map(|hook| {
                    convert_hook(
                        hook,
                        crate::registry::HOOK_STATUS_TIMEOUT_MS,
                        crate::registry::HOOK_STATUS_TIMEOUT_MAX_MS,
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

    Ok(Service {
        id: definition.id,
        kind: Kind::Core,
        version: definition.version,
        desc: definition.desc,
        deps: definition.dependencies,
        caps: Vec::new(),
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
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "svcd-definition-{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }

        fn services_dir(&self) -> PathBuf {
            self.0.join("services.d")
        }

        fn write(&self, name: &str, definition: Value) {
            fs::create_dir_all(self.services_dir()).expect("create services.d");
            fs::write(
                self.services_dir().join(format!("{}.json", name)),
                serde_json::to_vec_pretty(&definition).expect("serialize definition"),
            )
            .expect("write definition");
        }

        fn disable(&self, id: &str) {
            fs::create_dir_all(self.services_dir()).expect("create services.d");
            fs::write(self.services_dir().join(format!("{}.disabled", id)), "")
                .expect("write disabled marker");
        }

        fn write_script(&self, name: &str, body: &str) {
            fs::create_dir_all(self.services_dir()).expect("create services.d");
            let path = self.services_dir().join(name);
            fs::write(&path, body).expect("write script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .expect("chmod script");
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn absolute_exec() -> String {
        if cfg!(windows) {
            r"C:\Windows\System32\cmd.exe".to_string()
        } else {
            "/bin/sh".to_string()
        }
    }

    fn script_name(id: &str) -> String {
        if cfg!(windows) {
            format!("{}.cmd", id)
        } else {
            format!("{}.sh", id)
        }
    }

    fn all_actions_body() -> &'static str {
        #[cfg(windows)]
        {
            "@echo off\r\n:start\r\n:stop\r\n:status\r\n"
        }
        #[cfg(not(windows))]
        {
            "start() { :; }\nstop() { :; }\nstatus() { :; }\n"
        }
    }

    fn start_only_body() -> &'static str {
        #[cfg(windows)]
        {
            "@echo off\r\n:start\r\n"
        }
        #[cfg(not(windows))]
        {
            "start() { :; }\n"
        }
    }

    fn definition(id: &str) -> Value {
        json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "demo service",
            "protocol": 1,
            "exec": { "path": absolute_exec(), "args": [], "ready": "exec" },
            "dependencies": []
        })
    }

    #[test]
    fn load_issue_display_names_the_file_or_the_directory() {
        let issue = LoadIssue {
            file: Some("demo.json".into()),
            id: "demo".into(),
            reason: "bad definition".into(),
        };
        assert_eq!(issue.to_string(), "services.d/demo.json: bad definition");

        let issue = LoadIssue {
            file: None,
            id: String::new(),
            reason: "cannot read services.d".into(),
        };
        assert_eq!(issue.to_string(), "services.d: cannot read services.d");
    }

    #[test]
    fn loads_a_core_definition_next_to_the_builtin_svc() {
        let dir = TempDir::new();
        dir.write("demo", definition("demo"));

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        let demo = loaded.registry.get("demo").expect("demo is registered");
        assert_eq!(demo.kind, Kind::Core);
        assert_eq!(demo.state, State::Stopped);
        assert_eq!(demo.version, "1.0.0");
        assert_eq!(demo.deps, Vec::<String>::new());
        assert!(demo.caps.is_empty());
        assert!(demo.enabled);
        assert_eq!(demo.exec.as_ref().expect("exec").ready, Ready::Exec);
        let svc = loaded.registry.get(RESERVED_ID).expect("built-in svc");
        assert_eq!(svc.kind, Kind::Core);
        assert_eq!(svc.state, State::Running);
    }

    #[test]
    fn a_missing_services_dir_still_loads_the_builtin() {
        let dir = TempDir::new();
        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty());
        assert_eq!(loaded.registry.values().count(), 1);
        assert!(loaded.registry.get(RESERVED_ID).is_some());
    }

    #[test]
    fn a_disabled_marker_turns_off_enabled() {
        let dir = TempDir::new();
        dir.write("demo", definition("demo"));
        dir.disable("demo");

        let loaded = load_services(&dir.services_dir());

        let demo = loaded.registry.get("demo").expect("demo");
        assert!(!demo.enabled);
    }

    #[test]
    fn ignores_unknown_fields() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["future_field"] = json!({"anything": true});
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        assert!(loaded.registry.get("demo").is_some());
    }

    #[test]
    fn rejects_every_spk_only_field() {
        for field in ["platform", "files", "hooks", "config", "ui"] {
            let dir = TempDir::new();
            let mut body = definition("demo");
            body[field] = json!("anything");
            dir.write("demo", body);

            let loaded = load_services(&dir.services_dir());

            assert!(
                loaded.registry.get("demo").is_none(),
                "{} should be rejected",
                field
            );
            assert_eq!(loaded.issues.len(), 1, "{}: {:?}", field, loaded.issues);
            assert!(
                loaded.issues[0].reason.contains(field),
                "{} reason: {}",
                field,
                loaded.issues[0].reason
            );
            assert!(
                loaded.issues[0].reason.contains(".spk-only"),
                "{} reason: {}",
                field,
                loaded.issues[0].reason
            );
        }
    }

    #[test]
    fn rejects_the_reserved_svc_id() {
        let dir = TempDir::new();
        dir.write(RESERVED_ID, definition(RESERVED_ID));

        let loaded = load_services(&dir.services_dir());

        assert_eq!(loaded.registry.values().count(), 1);
        assert_eq!(
            loaded.registry.get(RESERVED_ID).expect("builtin").state,
            State::Running
        );
        assert!(loaded.issues[0].reason.contains("reserved"));
        assert_eq!(
            loaded.issues[0].to_string(),
            format!(
                "services.d/{}.json: {}",
                RESERVED_ID, loaded.issues[0].reason
            )
        );
    }

    #[test]
    fn trusts_the_json_id_over_the_file_name() {
        let dir = TempDir::new();
        dir.write("some-file", definition("demo"));

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        assert!(loaded.registry.get("demo").is_some());
    }

    #[test]
    fn rejects_two_files_that_claim_the_same_id() {
        let dir = TempDir::new();
        dir.write("a", definition("demo"));
        dir.write("b", definition("demo"));

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("demo").is_some());
        assert_eq!(loaded.issues.len(), 1, "issues: {:?}", loaded.issues);
        assert_eq!(loaded.issues[0].file.as_deref(), Some("b.json"));
        assert!(loaded.issues[0].reason.contains("duplicate service id"));
    }

    #[test]
    fn rejects_a_relative_exec_path() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["exec"]["path"] = json!("payload/demo");
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("demo").is_none());
        assert!(loaded.issues[0].reason.contains("must be absolute"));
    }

    #[test]
    fn rejects_bad_schema_protocol_and_ready() {
        for (field, value, reason) in [
            ("schema", json!(2), "unsupported schema"),
            ("protocol", json!(2), "unsupported protocol"),
        ] {
            let dir = TempDir::new();
            let mut body = definition("demo");
            body[field] = value;
            dir.write("demo", body);

            let loaded = load_services(&dir.services_dir());

            assert_eq!(loaded.issues.len(), 1, "{}: {:?}", field, loaded.issues);
            assert!(
                loaded.issues[0].reason.contains(reason),
                "{}: {}",
                field,
                loaded.issues[0].reason
            );
        }

        let dir = TempDir::new();
        let mut body = definition("demo");
        body["exec"]["ready"] = json!("later");
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues[0].reason.contains("exec.ready"));
    }

    #[test]
    fn rejects_core_capabilities() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["capabilities"] = json!([{ "name": "status", "desc": "a", "inputSchema": {} }]);
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("demo").is_none());
        assert!(loaded.issues[0].reason.contains("plugin-only"));
    }

    #[test]
    fn parses_lifecycle_hooks_and_defaults_their_timeouts() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["lifecycle"] = json!({
            "start": { "path": absolute_exec(), "args": ["start"], "timeout": 20 },
            "stop": { "path": absolute_exec(), "args": ["stop"] },
            "status": { "path": absolute_exec(), "args": ["status"] }
        });
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        let lifecycle = loaded
            .registry
            .get("demo")
            .expect("demo")
            .lifecycle
            .as_ref()
            .expect("lifecycle");
        assert_eq!(lifecycle.start.as_ref().expect("start").args, ["start"]);
        assert_eq!(lifecycle.start.as_ref().expect("start").timeout_ms, 20_000);
        assert_eq!(lifecycle.stop.as_ref().expect("stop").timeout_ms, 10_000);
        assert_eq!(lifecycle.status.as_ref().expect("status").timeout_ms, 5_000);
    }

    #[test]
    fn rejects_a_relative_lifecycle_path() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["lifecycle"] = json!({"stop": {"path": "scripts/stop"}});
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("demo").is_none());
        assert!(loaded.issues[0].reason.contains("must be absolute"));
    }

    #[test]
    fn allows_a_dependency_that_is_not_installed_yet() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["dependencies"] = json!(["not-installed"]);
        dir.write("demo", body);

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        assert!(loaded.registry.get("demo").is_some());
    }

    #[test]
    fn rejects_a_dependency_cycle_and_drops_only_its_members() {
        let dir = TempDir::new();
        let mut a = definition("a");
        a["dependencies"] = json!(["b"]);
        dir.write("a", a);
        let mut b = definition("b");
        b["dependencies"] = json!(["a"]);
        dir.write("b", b);
        dir.write("c", definition("c"));

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("a").is_none());
        assert!(loaded.registry.get("b").is_none());
        assert!(loaded.registry.get("c").is_some());
        let cycle_issues: Vec<&LoadIssue> = loaded
            .issues
            .iter()
            .filter(|issue| issue.reason.contains("dependency cycle"))
            .collect();
        assert_eq!(cycle_issues.len(), 2);
        assert!(cycle_issues[0].reason.contains("a -> b -> a"));
    }

    #[test]
    fn loads_a_drop_in_script_as_a_core_service() {
        let dir = TempDir::new();
        dir.write_script(&script_name("demo"), all_actions_body());

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        let demo = loaded.registry.get("demo").expect("demo");
        assert_eq!(demo.kind, Kind::Core);
        assert!(demo.caps.is_empty());
        assert_eq!(demo.version, "1.0.0");
        assert!(demo.enabled);
        let exec = demo.exec.as_ref().expect("exec");
        assert_eq!(exec.ready, Ready::Exec);
        assert_eq!(exec.args, ["start"]);
        let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
        assert_eq!(lifecycle.start.as_ref().expect("start").args, ["start"]);
        assert_eq!(lifecycle.stop.as_ref().expect("stop").args, ["stop"]);
        assert_eq!(lifecycle.status.as_ref().expect("status").args, ["status"]);
        assert_eq!(lifecycle.start.as_ref().expect("start").timeout_ms, 30_000);
        assert_eq!(lifecycle.stop.as_ref().expect("stop").timeout_ms, 10_000);
        assert_eq!(lifecycle.status.as_ref().expect("status").timeout_ms, 5_000);
    }

    #[test]
    fn a_script_without_actions_runs_from_the_top() {
        let dir = TempDir::new();
        dir.write_script(&script_name("demo"), "#!/bin/sh\r\n");

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        let demo = loaded.registry.get("demo").expect("demo");
        let exec = demo.exec.as_ref().expect("exec");
        assert!(exec.args.is_empty(), "exec args: {:?}", exec.args);
        assert!(demo.lifecycle.is_none());
    }

    #[test]
    fn only_declared_actions_become_lifecycle_hooks() {
        let dir = TempDir::new();
        dir.write_script(&script_name("demo"), start_only_body());

        let loaded = load_services(&dir.services_dir());

        let demo = loaded.registry.get("demo").expect("demo");
        assert_eq!(demo.exec.as_ref().expect("exec").args, ["start"]);
        let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
        assert!(lifecycle.start.is_some());
        assert!(lifecycle.stop.is_none());
        assert!(lifecycle.status.is_none());
    }

    #[test]
    fn actions_in_comments_are_not_declared() {
        let dir = TempDir::new();
        #[cfg(windows)]
        let body = "@echo off\r\nrem :start\r\n:: :stop\r\n";
        #[cfg(not(windows))]
        let body = "#!/bin/sh\n# start() { :; }\n# stop() { :; }\n# status() { :; }\n";
        dir.write_script(&script_name("demo"), body);

        let loaded = load_services(&dir.services_dir());

        let demo = loaded.registry.get("demo").expect("demo");
        assert!(demo.exec.as_ref().expect("exec").args.is_empty());
        assert!(demo.lifecycle.is_none(), "{:?}", demo.lifecycle);
    }

    #[cfg(unix)]
    #[test]
    fn a_case_branch_declares_a_shell_action() {
        let dir = TempDir::new();
        dir.write_script(
            &script_name("demo"),
            "#!/bin/sh\ncase \"$1\" in\n  start)\n    :;;\n  status)\n    :;;\nesac\n",
        );

        let loaded = load_services(&dir.services_dir());

        let demo = loaded.registry.get("demo").expect("demo");
        assert_eq!(demo.exec.as_ref().expect("exec").args, ["start"]);
        let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
        assert!(lifecycle.start.is_some());
        assert!(lifecycle.status.is_some());
        assert!(lifecycle.stop.is_none());
    }

    #[test]
    fn a_disabled_marker_turns_off_a_script() {
        let dir = TempDir::new();
        dir.write_script(&script_name("demo"), "");
        dir.disable("demo");

        let loaded = load_services(&dir.services_dir());

        assert!(!loaded.registry.get("demo").expect("demo").enabled);
    }

    #[test]
    fn a_json_definition_wins_over_a_script_with_the_same_id() {
        let dir = TempDir::new();
        dir.write("demo", definition("demo"));
        dir.write_script(&script_name("demo"), "");

        let loaded = load_services(&dir.services_dir());

        let demo = loaded.registry.get("demo").expect("demo");
        assert!(demo.lifecycle.is_none());
        assert!(
            loaded
                .issues
                .iter()
                .any(|issue| issue.reason.contains("already defined")),
            "issues: {:?}",
            loaded.issues
        );
    }

    #[test]
    fn rejects_invalid_and_reserved_script_ids() {
        let dir = TempDir::new();
        dir.write_script(&script_name("bad.name"), "");
        dir.write_script(&script_name("svc"), "");

        let loaded = load_services(&dir.services_dir());

        assert_eq!(loaded.issues.len(), 2, "issues: {:?}", loaded.issues);
        assert!(loaded
            .issues
            .iter()
            .any(|issue| issue.reason.contains("invalid")));
        assert!(loaded
            .issues
            .iter()
            .any(|issue| issue.reason.contains("reserved")));
    }

    #[cfg(windows)]
    #[test]
    fn loads_a_powershell_script() {
        let dir = TempDir::new();
        dir.write_script("demo.ps1", "function Start { }\r\nfunction Stop { }\r\n");

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
        let demo = loaded.registry.get("demo").expect("demo");
        assert_eq!(demo.exec.as_ref().expect("exec").args, ["start"]);
        let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
        assert!(lifecycle.start.is_some());
        assert!(lifecycle.stop.is_some());
        assert!(lifecycle.status.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_non_executable_script() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new();
        let name = script_name("demo");
        dir.write_script(&name, "");
        let path = dir.services_dir().join(&name);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("demo").is_none());
        assert!(loaded.issues[0].reason.contains("executable"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_script() {
        let dir = TempDir::new();
        let target = dir.0.join("target.sh");
        fs::write(&target, "").expect("target");
        fs::create_dir_all(dir.services_dir()).expect("services.d");
        std::os::unix::fs::symlink(&target, dir.services_dir().join("demo.sh")).expect("symlink");

        let loaded = load_services(&dir.services_dir());

        assert!(loaded.registry.get("demo").is_none());
        assert!(loaded.issues[0].reason.contains("symlink"));
    }
}
