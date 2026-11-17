use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::kernel::contract::{
    Capability as KernelCapability, CapabilityName, Contract, Effect, ImplRef,
    Limits as KernelLimits, Obligations, ServiceId,
};
use crate::kernel::failure::Class;

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

/// Crash-restart policy. `enabled` only decides whether a service starts at
/// boot; this decides what happens after it exits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// Restart after any exit, including a clean one.
    Always,
    /// Restart only after a failed exit (non-zero code or signal).
    #[default]
    OnFailure,
    /// Never restart; a clean or failed exit both stop the service.
    Never,
}

impl Restart {
    pub fn as_str(self) -> &'static str {
        match self {
            Restart::Always => "always",
            Restart::OnFailure => "on-failure",
            Restart::Never => "never",
        }
    }
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
    /// The only description of the service's callable surface. Built at load;
    /// runtime code reads this instead of a second capability vector.
    pub contract: Contract,
    /// The runtime carrier selected by the assembly point. `None` keeps the
    /// supervised-slot path; the builtin `svc` names `tag=builtin`.
    pub impl_ref: Option<ImplRef>,
    /// Used by the supervisor when it spawns a service (core or plugin).
    pub exec: Option<Exec>,
    /// Optional lifecycle commands; core definitions and schema-2 plugins use them.
    pub lifecycle: Option<Lifecycle>,
    /// Boot-start flag (`[auto]` in `svc list`); not the crash-restart policy.
    pub enabled: bool,
    /// Crash-restart policy; independent of the boot flag.
    pub restart: Restart,
    /// A one-shot service: a *successful* exit means "the work is done", so it
    /// stays `Running` with no pid (systemd's `RemainAfterExit`). `svc start`
    /// runs it again and `svc stop` only runs the stop action.
    pub remain_after_exit: bool,
    /// Device compatibility list (schema-3 plugins); `None` means universal.
    pub compat: Option<crate::platform::compat::CompatSpec>,
    pub state: State,
    pub pid: Option<u32>,
    pub since: Option<u64>,
    pub ready_since: Option<u64>,
    pub next_at: Option<u64>,
    pub failures: u32,
    pub last_exit: Option<i32>,
    pub config: Option<String>,
}

/// The runtime carrier of a service: exactly one owner of its runtime
/// resources (ADR-0015).
///
/// A service is either slot-backed (`exec`, the supervised process) or
/// launcher-backed (`impl_ref`, a registered tag such as `builtin`). A service
/// with neither is still supervised and listed but cannot be called; that shape
/// is legal, a service with both is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Carrier {
    /// The supervised slot owns the process (`exec`).
    Slot,
    /// A launcher owns the implementation (`impl_ref.tag`).
    Launcher,
    /// No callable carrier; only supervision applies.
    None,
}

/// Why a service has no single runtime carrier. The only illegal shape today is
/// both carriers at once; keep it typed so the rule is testable on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CarrierError {
    /// `exec` and `impl_ref` are both set: two owners for one service.
    Conflict,
}

impl std::fmt::Display for CarrierError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CarrierError::Conflict => write!(
                formatter,
                "declares both exec and impl_ref; a service is either slot-backed or launcher-backed"
            ),
        }
    }
}

/// Build the one callable description for a service at load time. The managed
/// `Contract` is immutable for the life of the entry; runtime code never
/// constructs a second surface.
///
/// The carrier decides the failure set: a launcher-backed service is the
/// builtin today, which can report every class; a supervised slot cannot
/// produce `Cancelled` (the caller drives cancellation). Capability
/// `effect`/`idempotent` stay with the caller that declares the surface; the
/// manifest has no way to express them, so the plugin loader projects the
/// conservative `Write` / non-idempotent pair.
pub(crate) fn contract_for(
    id: &str,
    version: &str,
    caps: Vec<KernelCapability>,
    carrier: Carrier,
    limits: &crate::platform::limits::Limits,
) -> Contract {
    let failures = match carrier {
        Carrier::Launcher => Class::ALL.to_vec(),
        // Cancelled is caller-driven only; a plugin that returns it is
        // normalized to ImplementationError before it reaches the contract.
        Carrier::Slot | Carrier::None => Class::ALL
            .into_iter()
            .filter(|class| *class != Class::Cancelled)
            .collect(),
    };
    let contract = Contract::new(
        ServiceId::new(id),
        version,
        caps,
        Obligations::default(),
        failures,
        KernelLimits {
            default_timeout: limits.default_timeout,
            max_timeout: limits.max_timeout,
            ..KernelLimits::default()
        },
    );
    debug_assert!(
        contract.validate().is_ok(),
        "invalid contract for service {}",
        id
    );
    contract
}

impl Service {
    /// ADR-0015's invariant: `exec` and `impl_ref` are mutually exclusive.
    /// Both empty is allowed (the service is supervised but not callable).
    pub fn carrier(&self) -> Result<Carrier, CarrierError> {
        match (self.exec.is_some(), self.impl_ref.is_some()) {
            (true, false) => Ok(Carrier::Slot),
            (false, true) => Ok(Carrier::Launcher),
            (false, false) => Ok(Carrier::None),
            (true, true) => Err(CarrierError::Conflict),
        }
    }

    /// The in-process `svc` service. It is always running and never enters the
    /// supervisor state machine. It is launcher-backed, so its contract uses
    /// the launcher failure set and default obligations: there is no supervised
    /// process to drive hooks or probes.
    pub fn builtin_svc(limits: &crate::platform::limits::Limits) -> Service {
        let caps = vec![
            KernelCapability {
                name: CapabilityName::new("version"),
                desc: "svcd version".into(),
                input_schema: Some(json!({"type": "object", "properties": {}})),
                effect: Effect::Read,
                idempotent: true,
            },
            KernelCapability {
                name: CapabilityName::new("health"),
                desc: "svcd health".into(),
                input_schema: Some(json!({"type": "object", "properties": {}})),
                effect: Effect::Read,
                idempotent: true,
            },
        ];
        Service {
            id: RESERVED_ID.into(),
            kind: Kind::Core,
            version: env!("CARGO_PKG_VERSION").into(),
            protocol: 1,
            desc: "core service management".into(),
            deps: Vec::new(),
            contract: contract_for(
                RESERVED_ID,
                env!("CARGO_PKG_VERSION"),
                caps,
                Carrier::Launcher,
                limits,
            ),
            impl_ref: Some(ImplRef {
                tag: "builtin".into(),
                binding: json!({}),
            }),
            exec: None,
            lifecycle: None,
            enabled: true,
            restart: Restart::Never,
            remain_after_exit: false,
            compat: None,
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

    pub fn with_builtin(limits: &crate::platform::limits::Limits) -> Self {
        let mut registry = Registry::new();
        registry
            .insert(Service::builtin_svc(limits))
            .expect("the built-in svc id is inserted exactly once");
        registry
    }

    /// The only door for a new service. Everything that can produce a Service
    /// (definitions, drop-in scripts, plugins, the builtin) comes through here,
    /// so the carrier invariant is enforced once and cannot be bypassed by a new
    /// loader.
    pub fn insert(&mut self, service: Service) -> Result<(), String> {
        if self.services.contains_key(&service.id) {
            return Err(format!("duplicate service id \"{}\"", service.id));
        }
        self.check_carrier(&service)?;
        self.check_identity(&service)?;
        self.services.insert(service.id.clone(), service);
        Ok(())
    }

    /// Replace an existing service with the same validation as insert. The
    /// registry is left untouched when the replacement is rejected, so a reload
    /// can log and skip instead of dropping a live service.
    pub fn replace(&mut self, service: Service) -> Result<(), String> {
        self.check_carrier(&service)?;
        self.check_identity(&service)?;
        self.services.insert(service.id.clone(), service);
        Ok(())
    }

    fn check_carrier(&self, service: &Service) -> Result<(), String> {
        service
            .carrier()
            .map(|_| ())
            .map_err(|error| format!("service \"{}\" {}", service.id, error))
    }

    /// The contract is the callable identity, not a parallel copy: a registry
    /// entry whose contract names another service would route calls and
    /// diagnostics to the wrong id.
    fn check_identity(&self, service: &Service) -> Result<(), String> {
        if service.contract.id.as_str() != service.id {
            return Err(format!(
                "service \"{}\": contract names \"{}\"",
                service.id,
                service.contract.id.as_str()
            ));
        }
        if service.contract.version != service.version {
            return Err(format!(
                "service \"{}\": contract version \"{}\" does not match \"{}\"",
                service.id, service.contract.version, service.version
            ));
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<Service> {
        self.services.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&Service> {
        self.services.get(id)
    }

    /// Mutable access for flags that are not part of the carrier invariant
    /// (`enabled`, live state). Replacing a whole service goes through
    /// `replace` so the invariant still holds.
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
    fn a_contract_carries_the_carrier_effect_and_failure_set() {
        let cap = KernelCapability {
            name: CapabilityName::new("run"),
            desc: "run it".into(),
            input_schema: None,
            effect: Effect::Write,
            idempotent: false,
        };
        let slot = contract_for(
            "demo",
            "1.0.0",
            vec![cap.clone()],
            Carrier::Slot,
            &crate::platform::limits::test_default(),
        );
        let slot_cap = &slot.surface()[0];
        assert_eq!(slot_cap.effect, Effect::Write);
        assert!(!slot_cap.idempotent);
        assert!(!slot.failures.contains(&Class::Cancelled));
        assert_eq!(slot.failures.len(), Class::ALL.len() - 1);
        assert_eq!(
            slot.limits.max_in_flight,
            crate::kernel::contract::MAX_IN_FLIGHT
        );

        let read_cap = KernelCapability {
            effect: Effect::Read,
            idempotent: true,
            ..cap
        };
        let builtin = contract_for(
            "svc",
            "1.0.0",
            vec![read_cap],
            Carrier::Launcher,
            &crate::platform::limits::test_default(),
        );
        let builtin_cap = &builtin.surface()[0];
        assert_eq!(builtin_cap.effect, Effect::Read);
        assert!(builtin_cap.idempotent);
        assert_eq!(builtin.failures, Class::ALL.to_vec());
    }

    #[test]
    fn the_builtin_service_declares_version_and_health() {
        let service = Service::builtin_svc(&crate::platform::limits::test_default());
        let names: Vec<&str> = service
            .contract
            .surface()
            .iter()
            .map(|cap| cap.name.as_str())
            .collect();
        assert_eq!(names, vec!["health", "version"]);
        assert!(service
            .contract
            .surface()
            .iter()
            .all(|cap| cap.effect == Effect::Read && cap.idempotent));
        assert_eq!(service.contract.failures, Class::ALL.to_vec());
    }

    #[test]
    fn find_cycle_reports_a_path() {
        let mut registry = Registry::new();
        let mut a = slot_backed("a");
        a.deps = vec!["b".into()];
        registry.insert(a).expect("insert a");
        let mut b = slot_backed("b");
        b.deps = vec!["a".into()];
        registry.insert(b).expect("insert b");

        let cycle = registry.find_cycle().expect("cycle");
        assert_eq!(cycle, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn duplicate_insert_is_rejected() {
        let mut registry = Registry::new();
        registry.insert(slot_backed("demo")).expect("first insert");

        let error = registry
            .insert(slot_backed("demo"))
            .expect_err("duplicate id");

        assert!(error.contains("duplicate service id"));
    }

    #[test]
    fn insert_rejects_a_contract_for_another_service() {
        let mut service = slot_backed("a");
        service.id = "b".into();

        let error = Registry::new()
            .insert(service)
            .expect_err("contract id must match the entry");

        assert!(error.contains("contract names"), "{}", error);
    }

    fn slot_backed(id: &str) -> Service {
        let mut service = Service::builtin_svc(&crate::platform::limits::test_default());
        let contract = contract_for(
            id,
            &service.version,
            service.contract.surface().to_vec(),
            Carrier::Slot,
            &crate::platform::limits::test_default(),
        );
        service.id = id.into();
        service.contract = contract;
        service.impl_ref = None;
        service.exec = Some(Exec {
            path: "/bin/true".into(),
            args: Vec::new(),
            env: Default::default(),
            ready: Ready::Exec,
            interpreter: None,
        });
        service
    }

    fn launcher_backed(id: &str) -> Service {
        let mut service = Service::builtin_svc(&crate::platform::limits::test_default());
        let contract = contract_for(
            id,
            &service.version,
            service.contract.surface().to_vec(),
            Carrier::Launcher,
            &crate::platform::limits::test_default(),
        );
        service.id = id.into();
        service.contract = contract;
        service.exec = None;
        service.impl_ref = Some(crate::kernel::contract::ImplRef {
            tag: "builtin".into(),
            binding: json!({}),
        });
        service
    }

    fn two_owners(id: &str) -> Service {
        Service {
            exec: Some(Exec {
                path: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
                interpreter: None,
            }),
            ..launcher_backed(id)
        }
    }

    #[test]
    fn every_carrier_shape_is_single_except_two_owners() {
        assert_eq!(slot_backed("core").carrier(), Ok(Carrier::Slot));
        assert_eq!(launcher_backed("built").carrier(), Ok(Carrier::Launcher));

        let neither = Service {
            exec: None,
            impl_ref: None,
            ..Service::builtin_svc(&crate::platform::limits::test_default())
        };
        assert_eq!(
            neither.carrier(),
            Ok(Carrier::None),
            "a service with neither carrier is legal but not callable"
        );

        assert_eq!(two_owners("bad").carrier(), Err(CarrierError::Conflict));
    }

    #[test]
    fn a_service_with_two_carriers_is_rejected_and_not_stored() {
        let mut registry = Registry::new();
        let error = registry
            .insert(two_owners("bad"))
            .expect_err("both carriers must be rejected");
        assert!(error.contains("both exec and impl_ref"), "{error}");
        assert!(registry.get("bad").is_none());
    }

    #[test]
    fn replace_rejects_a_conflict_and_keeps_the_previous_service() {
        let mut registry = Registry::new();
        registry.insert(slot_backed("demo")).expect("slot-backed");

        let error = registry
            .replace(two_owners("demo"))
            .expect_err("both carriers must be rejected");
        assert!(error.contains("both exec and impl_ref"), "{error}");
        assert_eq!(
            registry.get("demo").map(Service::carrier),
            Some(Ok(Carrier::Slot)),
            "a rejected replacement must leave the old service untouched"
        );
    }

    #[test]
    fn the_builtin_core_and_launcher_shapes_are_still_accepted() {
        let mut registry = Registry::with_builtin(&crate::platform::limits::test_default());
        registry.insert(slot_backed("core")).expect("core");
        registry.insert(launcher_backed("built")).expect("launcher");
        assert!(registry.get("svc").is_some());
        assert_eq!(
            registry.get("core").map(Service::carrier),
            Some(Ok(Carrier::Slot))
        );
    }
}
