use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::package::valid_name;
use crate::registry::{
    Exec, Kind, Ready, Registry, Service, State, RESERVED_CAP_PREFIX, RESERVED_ID,
};

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
    capabilities: Vec<CapabilityDefinition>,
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

#[derive(Deserialize)]
struct CapabilityDefinition {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    desc: String,
    #[serde(default, rename = "inputSchema")]
    #[allow(dead_code)]
    input_schema: Value,
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

    let mut seen_caps = BTreeSet::new();
    let mut caps = Vec::with_capacity(definition.capabilities.len());
    for capability in definition.capabilities {
        if capability.name.is_empty() {
            return Err("capability name must not be empty".into());
        }
        if capability.name.starts_with(RESERVED_CAP_PREFIX) {
            return Err(format!(
                "capability \"{}\" uses the reserved {} namespace",
                capability.name, RESERVED_CAP_PREFIX
            ));
        }
        if !seen_caps.insert(capability.name.clone()) {
            return Err(format!("duplicate capability \"{}\"", capability.name));
        }
        caps.push(capability.name);
    }

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
        caps,
        exec: Some(Exec {
            path: definition.exec.path,
            args: definition.exec.args,
            env: definition.exec.env,
            ready,
        }),
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

    fn definition(id: &str) -> Value {
        json!({
            "schema": 1,
            "id": id,
            "version": "1.0.0",
            "desc": "demo service",
            "protocol": 1,
            "exec": { "path": absolute_exec(), "args": [], "ready": "exec" },
            "dependencies": [],
            "capabilities": [{ "name": "status", "desc": "status", "inputSchema": {"type": "object"} }]
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
        assert_eq!(demo.caps, vec!["status".to_string()]);
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
    fn rejects_duplicate_or_reserved_capability_names() {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body["capabilities"] = json!([
            { "name": "status", "desc": "a", "inputSchema": {} },
            { "name": "status", "desc": "b", "inputSchema": {} }
        ]);
        dir.write("demo", body);
        let loaded = load_services(&dir.services_dir());
        assert!(loaded.issues[0].reason.contains("duplicate capability"));

        let dir = TempDir::new();
        let mut body = definition("demo");
        body["capabilities"] = json!([{ "name": "svc.list", "desc": "a", "inputSchema": {} }]);
        dir.write("demo", body);
        let loaded = load_services(&dir.services_dir());
        assert!(loaded.issues[0].reason.contains("reserved svc."));
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
}
