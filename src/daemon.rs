use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::api::{self, Api};
use crate::channel;
use crate::definition;
use crate::lan::LanStatus;
use crate::log;
use crate::package;
use crate::paths;
use crate::plugin::{self, Strategy};
use crate::registry::{Kind, Registry, Service, State, RESERVED_ID};
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

    if let Err(error) = log::init_svcd(&paths::logs_dir()) {
        eprintln!("svcd: cannot open log directory: {}", error);
    }
    log::svcd("svcd starting");

    let mut loaded = definition::load_services(&paths::services_dir());
    for issue in &loaded.issues {
        log::svcd(&format!("svcd: warning: {}", issue));
    }

    let (plugins, warnings) = plugin::scan();
    for warning in warnings {
        log::svcd(&format!("svcd: plugin: {}", warning));
    }
    for service in plugins {
        if loaded.registry.get(&service.id).is_some() {
            log::svcd(&format!(
                "svcd: plugin {} conflicts with an existing service; skipped",
                service.id
            ));
            continue;
        }
        if let Err(error) = loaded.registry.insert(service) {
            log::svcd(&format!("svcd: plugin: {}", error));
        }
    }

    let token = match channel::ensure_token(&config) {
        Ok(token) => token,
        Err(error) => {
            log::svcd(&format!("svcd: cannot create control token: {}", error));
            return 1;
        }
    };

    let (tx, rx) = mpsc::channel();
    if let Err(error) = channel::spawn_server(&config, tx) {
        log::svcd(&format!("svcd: cannot bind control channel: {}", error));
        return 1;
    }
    let api = match api::spawn(config.clone()) {
        Ok(api) => Some(api),
        Err(error) => {
            log::svcd(&format!("svcd: HTTP API disabled: {}", error));
            None
        }
    };
    notify_systemd("READY=1");

    let mut daemon = Daemon::new(loaded.registry, token, rx, api);
    install_shutdown_signal();
    daemon.start_enabled();
    daemon.event_loop();
    notify_systemd("STOPPING=1");
    channel::remove_socket(&config);
    0
}

/// systemd's `Type=notify` expects READY=1 once the management channel can
/// accept commands; services may still be starting. No-op without the
/// environment variable and on non-Unix hosts.
#[cfg(unix)]
fn notify_systemd(state: &str) {
    use std::os::unix::net::UnixDatagram;

    let Some(socket_path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if socket_path.is_empty() {
        return;
    }
    if let Ok(socket) = UnixDatagram::unbound() {
        let _ = socket.send_to(state.as_bytes(), socket_path);
    }
}

#[cfg(not(unix))]
fn notify_systemd(_state: &str) {}

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
    api: Option<Arc<Api>>,
    slots: BTreeMap<String, Slot>,
    shutdown: bool,
    started: Instant,
    epoch_at_start: SystemTime,
}

impl Daemon {
    fn new(
        registry: Registry,
        token: String,
        rx: Receiver<channel::PendingCall>,
        api: Option<Arc<Api>>,
    ) -> Self {
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
            api,
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
                log::svcd(&format!("svcd: start enabled: {}", error));
                return;
            }
        };
        for layer in layers {
            let deadline = self.now_ms().saturating_add(supervisor::START_TIMEOUT_MS);
            if let Err(error) = self.start_layer(&layer, deadline) {
                log::svcd(&format!("svcd: start enabled: {}", error));
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
            "plugin.install" => match self.install_plugin(&request) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => rpc::err(id, -32006, &error),
            },
            "plugin.uninstall" => match self.uninstall_plugin(&request) {
                Ok(()) => rpc::ok(id, json!({ "ok": true })),
                Err(error) => rpc::err(id, -32007, &error),
            },
            "lan.status" => match self.lan_status() {
                Ok(status) => rpc::ok(id, lan_json(&status, None)),
                Err(error) => rpc::err(id, -32008, &error),
            },
            "lan.enable" => {
                let bind = request
                    .params
                    .get("bind")
                    .and_then(Value::as_str)
                    .unwrap_or("0.0.0.0");
                match bind.parse::<IpAddr>() {
                    Ok(bind) => match self.lan_enable(bind) {
                        Ok((status, token)) => rpc::ok(id, lan_json(&status, token.as_deref())),
                        Err(error) => rpc::err(id, -32008, &error),
                    },
                    Err(_) => rpc::err(id, -32008, "invalid bind address"),
                }
            }
            "lan.disable" => match self.lan_disable() {
                Ok(status) => rpc::ok(id, lan_json(&status, None)),
                Err(error) => rpc::err(id, -32008, &error),
            },
            "lan.token.rotate" => match self.lan_rotate() {
                Ok(token) => rpc::ok(id, json!({ "token": token })),
                Err(error) => rpc::err(id, -32008, &error),
            },
            _ => rpc::err(id, -32601, "method not found"),
        }
    }

    fn lan_status(&self) -> Result<LanStatus, String> {
        self.api
            .as_ref()
            .ok_or_else(|| "HTTP API is not running".to_string())?
            .status()
    }

    fn lan_enable(&self, bind: IpAddr) -> Result<(LanStatus, Option<String>), String> {
        self.api
            .as_ref()
            .ok_or_else(|| "HTTP API is not running".to_string())?
            .enable(bind)
    }

    fn lan_disable(&self) -> Result<LanStatus, String> {
        self.api
            .as_ref()
            .ok_or_else(|| "HTTP API is not running".to_string())?
            .disable()
    }

    fn lan_rotate(&self) -> Result<String, String> {
        self.api
            .as_ref()
            .ok_or_else(|| "HTTP API is not running".to_string())?
            .rotate()
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
        if let Some(exec) = self
            .registry
            .get(id)
            .and_then(|service| service.exec.clone())
        {
            if let Some(slot) = self.slots.get_mut(id) {
                slot.set_exec(exec);
            }
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

    fn install_plugin(&mut self, request: &rpc::Request) -> Result<Status, String> {
        let path = request
            .params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "plugin.install requires path".to_string())?;
        let strategy = match Strategy::parse(
            request
                .params
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("keep-old"),
        ) {
            Some(strategy) => strategy,
            None => return Err("unknown strategy".into()),
        };

        let package = package::verify_package(Path::new(path))?;
        let id = package.manifest.name.clone();
        if let Some(existing) = self.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(format!("id is a core service: {}", id));
            }
        }

        let running = |slots: &BTreeMap<String, Slot>| {
            slots
                .get(&id)
                .map(|slot| !matches!(slot.machine.state, State::Stopped | State::Failed))
                .unwrap_or(false)
        };
        if running(&self.slots) && strategy == Strategy::Replace {
            self.stop_service(&id)?;
        }
        let still_running = running(&self.slots);
        let service = match plugin::install_package(&package, strategy) {
            Ok(service) => service,
            Err(error) if strategy == Strategy::KeepOld && is_no_space(&error) => {
                if still_running {
                    return Err(format!(
                        "not enough space for keep-old; stop the plugin and retry with replace: {}",
                        error
                    ));
                }
                log::svcd(&format!(
                    "svcd: plugin {}: keep-old ran out of space; falling back to replace",
                    id
                ));
                plugin::install_package(&package, Strategy::Replace)?
            }
            Err(error) => return Err(error),
        };
        if !still_running {
            self.slots.remove(&id);
            if let Some(exec) = service.exec.clone() {
                self.slots
                    .insert(id.clone(), Slot::new(id.clone(), exec, service.enabled));
            }
        }
        self.registry.remove(&id);
        self.registry.insert(service)?;
        log::svcd(&format!(
            "svcd: plugin {} {} installed ({})",
            id,
            package.manifest.version,
            if still_running {
                "running; next start uses"
            } else {
                "now"
            }
        ));
        self.status_for_id(&id)
            .ok_or_else(|| format!("unknown plugin: {}", id))
    }

    fn uninstall_plugin(&mut self, request: &rpc::Request) -> Result<(), String> {
        let id = request
            .params
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "plugin.uninstall requires id".to_string())?;
        let purge = request
            .params
            .get("purge")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let service = self
            .registry
            .get(id)
            .ok_or_else(|| format!("unknown plugin: {}", id))?;
        if service.kind != Kind::Plugin {
            return Err(format!("not a plugin: {}", id));
        }
        let running = self
            .slots
            .get(id)
            .map(|slot| !matches!(slot.machine.state, State::Stopped | State::Failed))
            .unwrap_or(false);
        if running {
            self.stop_service(id)?;
        }
        plugin::uninstall(id, purge)?;
        self.slots.remove(id);
        self.registry.remove(id);
        log::svcd(&format!("svcd: plugin {} uninstalled", id));
        Ok(())
    }

    fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<Status, String> {
        if self.registry.get(id).is_none() {
            return Err(format!("unknown service: {}", id));
        }
        if id == RESERVED_ID {
            return Err("the built-in svc service cannot be enabled or disabled".into());
        }
        let kind = self
            .registry
            .get(id)
            .map(|service| service.kind)
            .ok_or_else(|| format!("unknown service: {}", id))?;
        let (marker, create) = match kind {
            Kind::Plugin => (plugin::plugin_dir(id).join("enabled"), enabled),
            Kind::Core => (
                paths::services_dir().join(format!("{}.disabled", id)),
                !enabled,
            ),
        };
        if create {
            if let Some(parent) = marker.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
            }
            fs::write(&marker, b"")
                .map_err(|error| format!("cannot write {}: {}", marker.display(), error))?;
        } else if marker.exists() {
            fs::remove_file(&marker)
                .map_err(|error| format!("cannot remove {}: {}", marker.display(), error))?;
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

fn lan_json(status: &LanStatus, token: Option<&str>) -> Value {
    json!({
        "enabled": status.enabled,
        "bind": status.bind.to_string(),
        "tokenSet": status.token_set,
        "token": token,
    })
}

fn missing_dependencies(registry: &Registry, id: &str) -> Vec<String> {
    walk_dependencies(registry, &[id.to_string()]).1
}

fn is_no_space(error: &str) -> bool {
    error.contains("os error 28")
        || error.contains("os error 112")
        || error.contains("No space left")
        || error.contains("not enough space")
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
        let mut daemon = Daemon::new(registry, "token".into(), receiver, None);
        daemon.slots.get_mut("base").expect("base").machine.state = State::Failed;

        let error = daemon.start_service("app").expect_err("failed dependency");

        assert!(error.contains("dependency base is failed"), "{}", error);
    }
}
