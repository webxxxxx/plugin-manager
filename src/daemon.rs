use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::channel;
use crate::definition;
use crate::paths;
use crate::registry::{Registry, Service, State, RESERVED_ID};
use crate::rpc;
use crate::slot::Slot;
use crate::status::Status;
use crate::supervisor::{self, Event};

pub fn run() -> i32 {
    let config = paths::config_dir();
    if let Err(error) = fs::create_dir_all(&config) {
        eprintln!(
            "svcd: cannot create config directory {}: {}",
            config.display(),
            error
        );
        return 1;
    }

    let loaded = definition::load_services(&paths::services_dir());
    for issue in &loaded.issues {
        eprintln!("svcd: warning: {}", issue);
    }

    let token = match channel::ensure_token(&config) {
        Ok(token) => token,
        Err(error) => {
            eprintln!("svcd: cannot create control token: {}", error);
            return 1;
        }
    };

    let (tx, rx) = mpsc::channel();
    if let Err(error) = channel::spawn_server(&config, tx) {
        eprintln!("svcd: cannot bind control channel: {}", error);
        return 1;
    }

    let mut daemon = Daemon::new(loaded.registry, token, rx);
    install_shutdown_signal();
    daemon.start_enabled();
    daemon.event_loop();
    channel::remove_socket(&config);
    0
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn install_shutdown_signal() {
    extern "C" fn handle(_: libc::c_int) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, handle as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t);
    }
}

#[cfg(windows)]
fn install_shutdown_signal() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handle(_: u32) -> i32 {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
        1
    }

    unsafe {
        SetConsoleCtrlHandler(Some(handle), 1);
    }
}

pub fn shutdown() -> i32 {
    let config = paths::config_dir();
    match channel::request(&config, "svcd.shutdown", json!({})) {
        Ok(response) => match response.error {
            Some(error) => {
                eprintln!("svcd: {}", error.message);
                1
            }
            None => 0,
        },
        Err(error) => {
            eprintln!("svcd: svcd unreachable: {}", error);
            2
        }
    }
}

struct Daemon {
    registry: Registry,
    token: String,
    rx: Receiver<channel::PendingCall>,
    slots: BTreeMap<String, Slot>,
    shutdown: bool,
    started: Instant,
    epoch_at_start: SystemTime,
}

impl Daemon {
    fn new(registry: Registry, token: String, rx: Receiver<channel::PendingCall>) -> Self {
        let mut slots = BTreeMap::new();
        for service in registry.values() {
            if service.id == RESERVED_ID {
                continue;
            }
            if let Some(exec) = service.exec.clone() {
                slots.insert(
                    service.id.clone(),
                    Slot::new(service.id.clone(), exec, service.enabled),
                );
            }
        }
        Daemon {
            registry,
            token,
            rx,
            slots,
            shutdown: false,
            started: Instant::now(),
            epoch_at_start: SystemTime::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn start_enabled(&mut self) {
        let mut eligible = Vec::new();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            if !self.slots[&id].machine.enabled {
                continue;
            }
            let missing = missing_dependencies(&self.registry, &id);
            if missing.is_empty() {
                eligible.push(id);
            } else {
                eprintln!(
                    "svcd: warning: skipping {}: missing dependencies: {}",
                    id,
                    missing.join(", ")
                );
            }
        }
        if eligible.is_empty() {
            return;
        }
        let layers = match start_layers(&self.registry, &eligible) {
            Ok(layers) => layers,
            Err(error) => {
                eprintln!("svcd: start enabled: {}", error);
                return;
            }
        };
        for layer in layers {
            let deadline = self.now_ms().saturating_add(supervisor::START_TIMEOUT_MS);
            if let Err(error) = self.start_layer(&layer, deadline) {
                eprintln!("svcd: start enabled: {}", error);
            }
        }
    }

    fn event_loop(&mut self) {
        loop {
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                self.shutdown = true;
            }
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(incoming) => self.handle(incoming),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.pump();
            if self.shutdown {
                break;
            }
        }
        self.stop_all();
    }

    fn handle(&mut self, incoming: channel::PendingCall) {
        let response = match rpc::parse(&incoming.line) {
            Ok(request) => self.dispatch(request),
            Err(response) => response,
        };
        let _ = incoming.reply.send(rpc::to_line(&response));
    }

    fn dispatch(&mut self, request: rpc::Request) -> rpc::Response {
        if request.auth.as_deref() != Some(self.token.as_str()) {
            return rpc::err(request.id, -32000, "unauthorized");
        }
        let id = request.id.clone();
        match request.method.as_str() {
            "svcd.ping" => rpc::ok(id, json!({"ok": true})),
            "svcd.shutdown" => {
                self.shutdown = true;
                rpc::ok(id, json!({"ok": true}))
            }
            "svc.list" => rpc::ok(id, json!({"services": self.statuses()})),
            "svc.status" => match param_id(&request) {
                Ok(service_id) => match self.status_for_id(&service_id) {
                    Some(status) => rpc::ok(id, status_json(status)),
                    None => rpc::err(id, -32001, &format!("unknown service: {}", service_id)),
                },
                Err(response) => response,
            },
            "svc.start" => match param_id(&request) {
                Ok(service_id) => status_response(id, -32003, self.start_service(&service_id)),
                Err(response) => response,
            },
            "svc.stop" => match param_id(&request) {
                Ok(service_id) => status_response(id, -32004, self.stop_service(&service_id)),
                Err(response) => response,
            },
            "svc.enable" => match param_id(&request) {
                Ok(service_id) => status_response(id, -32005, self.set_enabled(&service_id, true)),
                Err(response) => response,
            },
            "svc.disable" => match param_id(&request) {
                Ok(service_id) => status_response(id, -32005, self.set_enabled(&service_id, false)),
                Err(response) => response,
            },
            _ => rpc::err(id, -32601, "method not found"),
        }
    }

    fn statuses(&self) -> Vec<Status> {
        self.registry
            .values()
            .map(|service| self.status_for(service))
            .collect()
    }

    fn status_for_id(&self, id: &str) -> Option<Status> {
        self.registry
            .get(id)
            .map(|service| self.status_for(service))
    }

    fn status_for(&self, service: &Service) -> Status {
        if service.id == RESERVED_ID {
            return Status::builtin(service);
        }
        match self.slots.get(&service.id) {
            Some(slot) => Status::live(service, &slot.machine, self.epoch_at_start),
            None => Status::of(service),
        }
    }

    fn start_service(&mut self, id: &str) -> Result<Status, String> {
        if self.registry.get(id).is_none() {
            return Err(format!("unknown service: {}", id));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id).expect("builtin"));
        }

        let layers = start_layers(&self.registry, &[id.to_string()])?;
        for dependency in layers
            .iter()
            .flatten()
            .filter(|service_id| service_id.as_str() != id)
        {
            match self.slots.get(dependency).map(|slot| slot.machine.state) {
                Some(State::Failed) => {
                    return Err(format!("dependency {} is failed", dependency));
                }
                Some(State::Backoff) => {
                    return Err(format!("dependency {} is backing off", dependency));
                }
                Some(State::Stopping) => {
                    return Err(format!("dependency {} is stopping", dependency));
                }
                _ => {}
            }
        }
        let deadline = self.now_ms().saturating_add(supervisor::START_TIMEOUT_MS);
        for layer in layers {
            self.start_layer(&layer, deadline)?;
        }

        self.status_for_id(id)
            .ok_or_else(|| format!("unknown service: {}", id))
    }

    fn start_layer(&mut self, ids: &[String], deadline: u64) -> Result<(), String> {
        let pending: Vec<String> = ids
            .iter()
            .filter(|id| self.slots.contains_key(*id))
            .filter(|id| self.slots[*id].machine.state != State::Running)
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        for id in &pending {
            if self.slots[id].machine.state == State::Stopping {
                return Err(format!("service {} is stopping", id));
            }
            let now = self.now_ms();
            self.slots
                .get_mut(id)
                .expect("slot")
                .apply(Event::Start, now);
        }

        loop {
            self.pump();
            let mut all_running = true;
            for id in &pending {
                match self.slots[id].machine.state {
                    State::Running => {}
                    State::Failed => {
                        return Err(format!("service {} failed to start", id));
                    }
                    _ => all_running = false,
                }
            }
            if all_running {
                return Ok(());
            }
            if self.now_ms() >= deadline {
                return Err(format!("start timed out: {}", pending.join(", ")));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<Status, String> {
        if self.registry.get(id).is_none() {
            return Err(format!("unknown service: {}", id));
        }
        if id == RESERVED_ID {
            return Err("the built-in svc service cannot be enabled or disabled".into());
        }
        let marker = paths::services_dir().join(format!("{}.disabled", id));
        if enabled {
            if marker.exists() {
                fs::remove_file(&marker)
                    .map_err(|error| format!("cannot remove {}: {}", marker.display(), error))?;
            }
        } else {
            if let Some(parent) = marker.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
            }
            fs::write(&marker, b"")
                .map_err(|error| format!("cannot write {}: {}", marker.display(), error))?;
        }
        if let Some(service) = self.registry.get_mut(id) {
            service.enabled = enabled;
        }
        if let Some(slot) = self.slots.get_mut(id) {
            slot.machine.enabled = enabled;
        }
        self.status_for_id(id)
            .ok_or_else(|| format!("unknown service: {}", id))
    }

    fn stop_service(&mut self, id: &str) -> Result<Status, String> {
        if self.registry.get(id).is_none() {
            return Err(format!("unknown service: {}", id));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id).expect("builtin"));
        }
        let state = self.slots.get(id).map(|slot| slot.machine.state);
        if matches!(state, None | Some(State::Stopped | State::Failed)) {
            return self
                .status_for_id(id)
                .ok_or_else(|| format!("unknown service: {}", id));
        }

        let now = self.now_ms();
        self.slots
            .get_mut(id)
            .expect("slot")
            .apply(Event::Stop, now);
        let deadline = now
            .saturating_add(supervisor::STOP_TIMEOUT_MS)
            .saturating_add(supervisor::KILL_TIMEOUT_MS)
            .saturating_add(1_000);
        loop {
            self.pump();
            match self.slots[id].machine.state {
                State::Stopped => break,
                State::Failed => return Err(format!("service {} did not stop", id)),
                _ => {}
            }
            if self.now_ms() >= deadline {
                return Err(format!("stop timed out: {}", id));
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.status_for_id(id)
            .ok_or_else(|| format!("unknown service: {}", id))
    }

    fn pump(&mut self) {
        let now = self.now_ms();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        for id in ids {
            let mut slot = self.slots.remove(&id).expect("slot");
            slot.pump(now);
            self.slots.insert(id, slot);
        }
    }

    fn stop_all(&mut self) {
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        let layers = start_layers(&self.registry, &ids).unwrap_or_else(|_| vec![ids]);
        let global_deadline = self.now_ms().saturating_add(supervisor::SHUTDOWN_BUDGET_MS);
        'layers: for layer in layers.into_iter().rev() {
            let active: Vec<String> = layer
                .into_iter()
                .filter(|id| {
                    self.slots
                        .get(id)
                        .map(|slot| {
                            matches!(
                                slot.machine.state,
                                State::Starting | State::Running | State::Stopping | State::Backoff
                            )
                        })
                        .unwrap_or(false)
                })
                .collect();
            if active.is_empty() {
                continue;
            }
            for id in &active {
                let now = self.now_ms();
                self.slots
                    .get_mut(id)
                    .expect("slot")
                    .apply(Event::Stop, now);
            }
            let deadline = self
                .now_ms()
                .saturating_add(supervisor::STOP_TIMEOUT_MS)
                .saturating_add(supervisor::KILL_TIMEOUT_MS)
                .min(global_deadline);
            loop {
                self.pump();
                if active.iter().all(|id| {
                    matches!(self.slots[id].machine.state, State::Stopped | State::Failed)
                }) {
                    break;
                }
                if self.now_ms() >= deadline {
                    if self.now_ms() >= global_deadline {
                        for slot in self.slots.values_mut() {
                            if !matches!(slot.machine.state, State::Stopped | State::Failed) {
                                slot.signal_force();
                            }
                        }
                        break 'layers;
                    }
                    for id in &active {
                        if !matches!(self.slots[id].machine.state, State::Stopped | State::Failed) {
                            if let Some(slot) = self.slots.get_mut(id) {
                                slot.signal_force();
                            }
                        }
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn status_response(
    id: Option<Value>,
    error_code: i32,
    result: Result<Status, String>,
) -> rpc::Response {
    match result {
        Ok(status) => match serde_json::to_value(status) {
            Ok(value) => rpc::ok(id, value),
            Err(error) => rpc::err(id, -32603, &error.to_string()),
        },
        Err(message) => rpc::err(id, error_code, &message),
    }
}

fn status_json(status: Status) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

fn param_id(request: &rpc::Request) -> Result<String, rpc::Response> {
    request
        .params
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| rpc::err(request.id.clone(), -32602, "missing id"))
}

fn start_layers(registry: &Registry, ids: &[String]) -> Result<Vec<Vec<String>>, String> {
    let (closure, missing) = walk_dependencies(registry, ids);
    if !missing.is_empty() {
        return Err(format!("missing dependencies: {}", missing.join(", ")));
    }

    let mut remaining = closure;
    let mut layers = Vec::new();
    while !remaining.is_empty() {
        let layer: Vec<String> = remaining
            .iter()
            .filter(|id| {
                registry
                    .get(id)
                    .map(|service| service.deps.iter().all(|dep| !remaining.contains(dep)))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        if layer.is_empty() {
            return Err("dependency cycle".into());
        }
        for id in &layer {
            remaining.remove(id);
        }
        layers.push(layer);
    }
    Ok(layers)
}

fn walk_dependencies(registry: &Registry, ids: &[String]) -> (BTreeSet<String>, Vec<String>) {
    let mut closure = BTreeSet::new();
    let mut stack = ids.to_vec();
    let mut missing = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if !closure.insert(id.clone()) {
            continue;
        }
        if let Some(service) = registry.get(&id) {
            for dependency in &service.deps {
                if registry.get(dependency).is_none() {
                    missing.insert(dependency.clone());
                } else {
                    stack.push(dependency.clone());
                }
            }
        }
    }
    (closure, missing.into_iter().collect())
}

fn missing_dependencies(registry: &Registry, id: &str) -> Vec<String> {
    walk_dependencies(registry, &[id.to_string()]).1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Exec, Kind, Ready, Service};

    fn service(id: &str, deps: &[&str]) -> Service {
        Service {
            id: id.into(),
            kind: Kind::Core,
            version: "1.0.0".into(),
            desc: "test".into(),
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
            caps: Vec::new(),
            exec: Some(Exec {
                path: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
            }),
            enabled: true,
            state: State::Stopped,
            pid: None,
            since: None,
            ready_since: None,
            next_at: None,
            failures: 0,
            last_exit: None,
            config: None,
        }
    }

    fn registry(services: Vec<Service>) -> Registry {
        let mut registry = Registry::new();
        for service in services {
            registry.insert(service).expect("insert");
        }
        registry
    }

    #[test]
    fn start_layers_puts_dependencies_in_earlier_layers() {
        let registry = registry(vec![
            service("app", &["base"]),
            service("base", &[]),
            service("independent", &[]),
        ]);

        assert_eq!(
            start_layers(&registry, &["app".into()]).expect("layers"),
            vec![vec!["base".to_string()], vec!["app".to_string()],]
        );
    }

    #[test]
    fn start_layers_groups_independent_services_together() {
        let registry = registry(vec![service("a", &[]), service("b", &[])]);

        assert_eq!(
            start_layers(&registry, &["a".into(), "b".into()]).expect("layers"),
            vec![vec!["a".to_string(), "b".to_string()]]
        );
    }

    #[test]
    fn start_layers_names_every_missing_dependency() {
        let registry = registry(vec![service("app", &["first", "second"])]);

        let error = start_layers(&registry, &["app".into()]).expect_err("missing");

        assert!(error.contains("missing dependencies:"));
        assert!(error.contains("first"));
        assert!(error.contains("second"));
    }

    #[test]
    fn missing_dependencies_lists_only_absent_ids() {
        let registry = registry(vec![service("app", &["base", "missing"])]);
        let mut registry = registry;
        registry.insert(service("base", &[])).expect("base");

        assert_eq!(
            missing_dependencies(&registry, "app"),
            vec!["missing".to_string()]
        );
    }

    #[test]
    fn missing_dependencies_walks_transitively() {
        let registry = registry(vec![
            service("app", &["middle"]),
            service("middle", &["missing"]),
        ]);

        assert_eq!(
            missing_dependencies(&registry, "app"),
            vec!["missing".to_string()]
        );
    }

    #[test]
    fn start_refuses_a_failed_dependency() {
        let registry = registry(vec![service("app", &["base"]), service("base", &[])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, "token".into(), receiver);
        daemon.slots.get_mut("base").expect("base").machine.state = State::Failed;

        let error = daemon.start_service("app").expect_err("failed dependency");

        assert!(error.contains("dependency base is failed"), "{}", error);
    }
}
