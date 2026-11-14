use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The built-in core service id; reserved against `services.d` definitions.
pub const RESERVED_ID: &str = "svc";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Core,
    /// Plugins enter the same registry once the .spk scanner lands (#30).
    #[allow(dead_code)]
    Plugin,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Core => "core",
            Kind::Plugin => "plugin",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Backoff,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Running => "running",
            State::Stopping => "stopping",
            State::Backoff => "backoff",
            State::Failed => "failed",
        }
    }

    /// Only a fully running service publishes its capability surface.
    #[allow(dead_code)]
    pub fn is_live(self) -> bool {
        matches!(self, State::Running)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ready {
    #[default]
    Notify,
    Exec,
}

/// Default and maximum budgets for a lifecycle action. The start action is
/// supervised under the state machine's start timeout; its own `timeout` is
/// kept for parity with a core definition and is not armed separately.
pub const HOOK_START_TIMEOUT_MS: u64 = 30_000;
pub const HOOK_STOP_TIMEOUT_MS: u64 = 10_000;
pub const HOOK_STATUS_TIMEOUT_MS: u64 = 5_000;
pub const HOOK_TIMEOUT_MAX_MS: u64 = 300_000;
pub const HOOK_STATUS_TIMEOUT_MAX_MS: u64 = 30_000;

/// A resolved host interpreter. `path` is absolute and was validated when the
/// plugin was installed, so the supervisor runs the same binary that passed the
/// version check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interpreter {
    pub path: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exec {
    pub path: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub ready: Ready,
    /// Present when `path` is a script the daemon must run through an interpreter.
    pub interpreter: Option<Interpreter>,
}

/// One lifecycle hook for a core service. `timeout_ms` is only used by stop
/// and status; the start command is supervised under the normal start timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    pub path: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_ms: u64,
    /// Present when `path` is a script the daemon must run through an interpreter.
    pub interpreter: Option<Interpreter>,
}

/// A capability advertised by a plugin manifest.
///
/// The aggregation layer passes `input_schema` through without validating it;
/// the plugin owns the schema.
#[derive(Clone, Debug, PartialEq)]
pub struct Capability {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Optional start/stop/status commands declared by a service definition or an
/// `.spk` manifest.
///
/// `start` replaces `exec` as the supervised foreground process; `stop` runs
/// before the normal polite stop sequence; `status` is a probe run by
/// `svc status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lifecycle {
    pub start: Option<Hook>,
    pub stop: Option<Hook>,
    pub status: Option<Hook>,
}

#[derive(Clone, Debug)]
pub struct Service {
    pub id: String,
    pub kind: Kind,
    pub version: String,
    /// Plugin protocol version: 1 = LF-delimited NDJSON, 2 = length-prefixed.
    pub protocol: u32,
    /// Kept from the definition; the status payload does not expose it yet.
    #[allow(dead_code)]
    pub desc: String,
    pub deps: Vec<String>,
    /// Only plugins declare capabilities; core services are lifecycle units.
    pub caps: Vec<Capability>,
    /// Used by the supervisor when it spawns a service (core or plugin).
    pub exec: Option<Exec>,
    /// Optional lifecycle commands; core definitions and schema-2 plugins use them.
    pub lifecycle: Option<Lifecycle>,
    /// Boot-start flag (`[auto]` in `svc list`); not the crash-restart policy.
    pub enabled: bool,
    pub state: State,
    pub pid: Option<u32>,
    pub since: Option<u64>,
    pub ready_since: Option<u64>,
    pub next_at: Option<u64>,
    pub failures: u32,
    pub last_exit: Option<i32>,
    pub config: Option<String>,
}

impl Service {
    /// The in-process `svc` service. It is always running and never enters the
    /// supervisor state machine.
    pub fn builtin_svc() -> Service {
        Service {
            id: RESERVED_ID.into(),
            kind: Kind::Core,
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: 1,
            desc: "core service management".into(),
            deps: Vec::new(),
            caps: Vec::new(),
            exec: None,
            lifecycle: None,
            enabled: true,
            state: State::Running,
            pid: None,
            since: None,
            ready_since: None,
            next_at: None,
            failures: 0,
            last_exit: None,
            config: None,
        }
    }
}

#[derive(Default)]
pub struct Registry {
    services: BTreeMap<String, Service>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    pub fn with_builtin() -> Self {
        let mut registry = Registry::new();
        registry
            .insert(Service::builtin_svc())
            .expect("the built-in svc id is inserted exactly once");
        registry
    }

    pub fn insert(&mut self, service: Service) -> Result<(), String> {
        if self.services.contains_key(&service.id) {
            return Err(format!("duplicate service id \"{}\"", service.id));
        }
        self.services.insert(service.id.clone(), service);
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<Service> {
        self.services.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&Service> {
        self.services.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Service> {
        self.services.get_mut(id)
    }

    pub fn values(&self) -> impl Iterator<Item = &Service> {
        self.services.values()
    }

    /// The services svcd supervises. The built-in `svc` is svcd itself, so it is
    /// not part of the list projection.
    pub fn managed_values(&self) -> impl Iterator<Item = &Service> {
        self.values().filter(|service| service.id != RESERVED_ID)
    }

    /// One cycle path (without the repeated head), used to reject definitions at
    /// load and to name the path in the diagnostic.
    pub fn find_cycle(&self) -> Option<Vec<String>> {
        const WHITE: u8 = 0;
        const GRAY: u8 = 1;
        const BLACK: u8 = 2;

        fn visit(
            id: &str,
            registry: &Registry,
            color: &mut BTreeMap<String, u8>,
            stack: &mut Vec<String>,
        ) -> Option<Vec<String>> {
            color.insert(id.to_string(), GRAY);
            stack.push(id.to_string());
            if let Some(service) = registry.services.get(id) {
                for dep in &service.deps {
                    if !registry.services.contains_key(dep) {
                        continue;
                    }
                    match color.get(dep).copied().unwrap_or(WHITE) {
                        WHITE => {
                            if let Some(cycle) = visit(dep, registry, color, stack) {
                                return Some(cycle);
                            }
                        }
                        GRAY => {
                            let start = stack.iter().position(|item| item == dep).unwrap_or(0);
                            return Some(stack[start..].to_vec());
                        }
                        _ => {}
                    }
                }
            }
            stack.pop();
            color.insert(id.to_string(), BLACK);
            None
        }

        let mut color: BTreeMap<String, u8> =
            self.services.keys().map(|id| (id.clone(), WHITE)).collect();
        let ids: Vec<String> = self.services.keys().cloned().collect();
        for id in ids {
            if color.get(&id).copied().unwrap_or(WHITE) == WHITE {
                let mut stack = Vec::new();
                if let Some(cycle) = visit(&id, self, &mut color, &mut stack) {
                    return Some(cycle);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_and_json_share_the_lowercase_names() {
        let states = [
            (State::Stopped, "stopped"),
            (State::Starting, "starting"),
            (State::Running, "running"),
            (State::Stopping, "stopping"),
            (State::Backoff, "backoff"),
            (State::Failed, "failed"),
        ];
        for (state, name) in states {
            assert_eq!(state.as_str(), name);
            assert_eq!(serde_json::to_value(state).expect("serialize"), json!(name));
        }
        assert_eq!(Kind::Core.as_str(), "core");
        assert_eq!(
            serde_json::to_value(Kind::Core).expect("serialize"),
            json!("core")
        );
        assert_eq!(Kind::Plugin.as_str(), "plugin");
        assert_eq!(
            serde_json::to_value(Kind::Plugin).expect("serialize"),
            json!("plugin")
        );
    }

    #[test]
    fn find_cycle_reports_a_path() {
        let mut registry = Registry::new();
        registry
            .insert(Service {
                id: "a".into(),
                deps: vec!["b".into()],
                ..Service::builtin_svc()
            })
            .expect("insert a");
        registry
            .insert(Service {
                id: "b".into(),
                deps: vec!["a".into()],
                ..Service::builtin_svc()
            })
            .expect("insert b");

        let cycle = registry.find_cycle().expect("cycle");
        assert_eq!(cycle, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn duplicate_insert_is_rejected() {
        let mut registry = Registry::new();
        registry
            .insert(Service {
                id: "demo".into(),
                ..Service::builtin_svc()
            })
            .expect("first insert");

        let error = registry
            .insert(Service {
                id: "demo".into(),
                ..Service::builtin_svc()
            })
            .expect_err("duplicate id");

        assert!(error.contains("duplicate service id"));
    }
}
