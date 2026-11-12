use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::IpAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::api::{self, Api};
use crate::channel;
use crate::definition;
use crate::lan::LanStatus;
use crate::log;
use crate::mcp;
use crate::package;
use crate::paths;
use crate::plugin::{self, Strategy};
use crate::registry::{Hook, Kind, Registry, Service, State, RESERVED_ID};
use crate::rpc;
use crate::slot::hook_command;
use crate::slot::{CallError, Slot};
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
    if let Err(error) = crate::proc::init() {
        log::svcd(&format!(
            "svcd: parent-death protection unavailable: {}",
            error
        ));
    }

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
    #[cfg(windows)]
    let _timer = TimerResolution::acquire();
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

#[cfg(windows)]
struct TimerResolution;

#[cfg(windows)]
impl TimerResolution {
    fn acquire() -> Self {
        // Windows rounds short sleeps up to the 15.6ms system timer by
        // default. The daemon's 1ms tick and the HTTP accept poll need a finer
        // granularity to keep control-plane latency low.
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        TimerResolution
    }
}

#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
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
                let mut slot = Slot::new(
                    service.id.clone(),
                    exec,
                    service.lifecycle.clone(),
                    service.enabled,
                );
                slot.set_protocol_only(service.kind == Kind::Plugin);
                slots.insert(service.id.clone(), slot);
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
            match self.rx.recv_timeout(Duration::from_millis(1)) {
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
        let request = match rpc::parse(&incoming.line) {
            Ok(request) => request,
            Err(response) => {
                let _ = incoming.reply.send(rpc::to_line(&response));
                return;
            }
        };
        if request.auth.as_deref() != Some(self.token.as_str()) {
            let response = rpc::err(request.id, -32000, "unauthorized");
            let _ = incoming.reply.send(rpc::to_line(&response));
            return;
        }
        match request.method.as_str() {
            "registry.call" | "tools/call" => self.handle_tool_call(request, incoming.reply),
            _ => {
                let response = self.dispatch(request);
                let _ = incoming.reply.send(rpc::to_line(&response));
            }
        }
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
            "registry.tools" | "tools/list" => rpc::ok(id, json!({ "tools": self.tool_list() })),
            "registry.version" => {
                rpc::ok(id, json!({ "version": tool_version(&self.tool_list()) }))
            }
            "svc.list" => rpc::ok(id, json!({"services": self.statuses()})),
            "svc.status" => match param_id(&request) {
                Ok(service_id) => match self.status_for_id(&service_id) {
                    Some(mut status) => {
                        if request
                            .params
                            .get("probe")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            self.probe_status(&service_id, &mut status);
                        }
                        rpc::ok(id, status_json(status))
                    }
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

    /// The MCP projection asks the daemon rather than the registry because the
    /// registry snapshot does not track the supervised process.
    fn is_live(&self, id: &str) -> bool {
        self.slots
            .get(id)
            .map(|slot| slot.machine.state == State::Running)
            .unwrap_or(false)
    }

    fn tool_list(&self) -> Vec<Value> {
        mcp::tools(&self.registry, |id| self.is_live(id))
    }

    /// Handle tools/call. Plugin calls are deferred: the plugin response arrives
    /// on the slot pump, so the control connection stays open without blocking
    /// the daemon's event loop. A start tool is synchronous because starting is
    /// defined as synchronous in the service contract.
    fn handle_tool_call(&mut self, request: rpc::Request, reply: Sender<String>) {
        let id = request.id.clone();
        let name = request
            .params
            .get("name")
            .or_else(|| request.params.get("tool"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(name) = name else {
            send(
                &reply,
                rpc::err(id, -32602, "tools/call requires params.name"),
            );
            return;
        };
        let arguments = match request.params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(value) if value.is_object() => value.clone(),
            Some(_) => {
                send(
                    &reply,
                    rpc::err(id, -32602, "tools/call arguments must be an object"),
                );
                return;
            }
        };

        // Only a known plugin's <id>.start is the lifecycle tool. A capability
        // named "foo.start" falls through to the capability lookup below.
        let start_id = name.strip_suffix(".start").filter(|service_id| {
            self.registry
                .get(service_id)
                .map(|service| service.kind == Kind::Plugin)
                .unwrap_or(false)
        });
        if let Some(service_id) = start_id {
            let response = match self.start_service(service_id) {
                Ok(status) => mcp::tool_result(id, status_json(status).to_string(), false),
                Err(error) => mcp::tool_error(id, &error),
            };
            send(&reply, response);
            return;
        }

        let Some((service_id, capability)) = name.split_once('.') else {
            send(&reply, unknown_tool(&id, &name));
            return;
        };
        let Some(service) = self.registry.get(service_id) else {
            send(&reply, unknown_tool(&id, &name));
            return;
        };
        if service.kind != Kind::Plugin || !service.caps.iter().any(|cap| cap.name == capability) {
            send(&reply, unknown_tool(&id, &name));
            return;
        }

        let now = self.now_ms();
        let Some(slot) = self.slots.get_mut(service_id) else {
            send(
                &reply,
                mcp::tool_result(id, format!("plugin {} has no runtime", service_id), true),
            );
            return;
        };
        match slot.request_capability(capability, arguments, id.clone(), now, reply.clone()) {
            Ok(()) => {}
            Err(CallError::Busy) => {
                send(
                    &reply,
                    rpc::err(id, -32000, "busy: too many in-flight tool calls"),
                );
            }
            Err(CallError::NotLive) => {
                send(
                    &reply,
                    mcp::tool_result(
                        id,
                        format!(
                            "plugin {} is not running; call {}.start first",
                            service_id, service_id
                        ),
                        true,
                    ),
                );
            }
            Err(CallError::TooLarge) => {
                // The caller's arguments are too large for the transport. The
                // plugin did nothing wrong, so this is a params error rather
                // than a plugin protocol violation.
                send(
                    &reply,
                    rpc::err(
                        id,
                        -32602,
                        "tools/call request exceeds the 1 MiB frame limit",
                    ),
                );
            }
            Err(CallError::Write(error)) => {
                send(
                    &reply,
                    mcp::tool_result(
                        id,
                        format!("cannot write to plugin {}: {}", service_id, error),
                        true,
                    ),
                );
            }
        }
    }

    /// Run the optional status hook. This is only called when the caller asks
    /// for a probe (`svc status`), not by `svc list` or the HTTP projections.
    fn probe_status(&mut self, id: &str, status: &mut Status) {
        let Some(hook) = self
            .registry
            .get(id)
            .and_then(|service| service.lifecycle.as_ref())
            .and_then(|lifecycle| lifecycle.status.as_ref())
            .cloned()
        else {
            return;
        };
        status.details.probe = Some(run_status_hook(id, &hook));
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
        if let Some(service) = self.registry.get(id) {
            let exec = service.exec.clone();
            let lifecycle = service.lifecycle.clone();
            if let Some(slot) = self.slots.get_mut(id) {
                if let Some(exec) = exec {
                    slot.set_exec(exec);
                }
                slot.set_lifecycle(lifecycle);
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
                let mut slot =
                    Slot::new(id.clone(), exec, service.lifecycle.clone(), service.enabled);
                slot.set_protocol_only(true);
                self.slots.insert(id.clone(), slot);
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
        let Some(state) = self.slots.get(id).map(|slot| slot.machine.state) else {
            return self
                .status_for_id(id)
                .ok_or_else(|| format!("unknown service: {}", id));
        };

        let now = self.now_ms();
        self.slots.get_mut(id).expect("slot").request_stop(now);
        if matches!(state, State::Stopped | State::Failed) {
            return self
                .status_for_id(id)
                .ok_or_else(|| format!("unknown service: {}", id));
        }
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
                self.slots.get_mut(id).expect("slot").request_stop(now);
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

fn tool_version(tools: &[Value]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    serde_json::to_string(tools)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn send(reply: &Sender<String>, response: rpc::Response) {
    let _ = reply.send(rpc::to_line(&response));
}

fn unknown_tool(id: &Option<Value>, name: &str) -> rpc::Response {
    rpc::err(id.clone(), -32602, &format!("unknown tool: {}", name))
}

fn run_status_hook(id: &str, hook: &Hook) -> String {
    let mut command = hook_command(id, "status", hook);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return format!("spawn failed: {}", error),
    };
    let _ = crate::proc::assign(&child);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return "ok".into();
                }
                return format!("failed ({})", status);
            }
            Ok(None) => {}
            Err(error) => return format!("error: {}", error),
        }
        if started.elapsed() >= Duration::from_millis(hook.timeout_ms) {
            let _ = child.kill();
            let _ = child.wait();
            return "timed out".into();
        }
        thread::sleep(Duration::from_millis(50));
    }
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
    use crate::registry::{Capability, Exec, Kind, Ready, Service};

    fn capability(name: &str) -> Capability {
        Capability {
            name: name.into(),
            description: format!("{} description", name),
            input_schema: json!({"type": "object"}),
        }
    }

    fn plugin(id: &str, caps: &[&str]) -> Service {
        Service {
            kind: Kind::Plugin,
            caps: caps.iter().map(|cap| capability(cap)).collect(),
            ..service(id, &[])
        }
    }

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
            lifecycle: None,
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

    fn tool_agent(root: &Path) -> Exec {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = root.join("agent.sh");
            std::fs::write(
                &script,
                r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *svcd.ping*) printf '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n' ;;
    *'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' ;;
    *'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"boom"}}\n' ;;
    *'"name":"hang"'*) ;;
    *'"name":"crash"'*) exit 0 ;;
  esac
done
"#,
            )
            .expect("script");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            Exec {
                path: "/bin/sh".into(),
                args: vec![script.to_string_lossy().to_string()],
                env: Default::default(),
                ready: Ready::Notify,
            }
        }
        #[cfg(windows)]
        {
            let script = root.join("agent.ps1");
            std::fs::write(
                &script,
                r#"$reader = [Console]::In
while ($null -ne ($line = $reader.ReadLine())) {
  $msg = $line | ConvertFrom-Json
  if ($msg.method -eq 'svcd.ping') {
    [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"ok":true}}')
    [Console]::Out.Flush()
    continue
  }
  if ($msg.method -eq 'tools/call') {
    $name = $msg.params.name
    if ($name -eq 'crash') { exit 0 }
    if ($name -eq 'hang') { continue }
    if ($name -eq 'echo') {
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}'
    } elseif ($name -eq 'fail') {
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"error":{"code":-32000,"message":"boom"}}'
    } else {
      continue
    }
    [Console]::Out.WriteLine($out)
    [Console]::Out.Flush()
  }
}
"#,
            )
            .expect("script");
            let powershell = std::env::var_os("SystemRoot")
                .map(|root| Path::new(&root).join("System32/WindowsPowerShell/v1.0/powershell.exe"))
                .filter(|path| path.is_file())
                .unwrap_or_else(|| std::path::PathBuf::from("powershell.exe"));
            Exec {
                path: powershell.to_string_lossy().to_string(),
                args: vec![
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-File".into(),
                    script.to_string_lossy().to_string(),
                ],
                env: Default::default(),
                ready: Ready::Notify,
            }
        }
    }

    fn send(daemon: &mut Daemon, method: &str, params: Value) -> Receiver<String> {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
            "auth": "token",
        }))
        .expect("request");
        let (sender, receiver) = mpsc::channel();
        daemon.handle(channel::PendingCall {
            line,
            reply: sender,
        });
        receiver
    }

    fn wait(daemon: &mut Daemon, receiver: &Receiver<String>) -> rpc::Response {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(line) = receiver.try_recv() {
                return serde_json::from_str(&line).expect("response json");
            }
            daemon.pump();
            assert!(Instant::now() < deadline, "no response");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_live_plugin_routes_tool_calls_and_fails_them_on_a_crash() {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("svcd-tools-unit-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("logs")).expect("logs");
        std::env::set_var("SVCD_LOG_DIR", root.join("logs"));

        let plugin = Service {
            id: "plug".into(),
            kind: Kind::Plugin,
            version: "1.0.0".into(),
            desc: "tools".into(),
            deps: Vec::new(),
            caps: vec![
                capability("echo"),
                capability("fail"),
                capability("hang"),
                capability("crash"),
            ],
            exec: Some(tool_agent(&root)),
            lifecycle: None,
            enabled: true,
            state: State::Stopped,
            pid: None,
            since: None,
            ready_since: None,
            next_at: None,
            failures: 0,
            last_exit: None,
            config: None,
        };
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![plugin]), "token".into(), receiver, None);
        daemon.start_service("plug").expect("start");
        assert!(daemon.is_live("plug"));

        let echo = send(
            &mut daemon,
            "tools/call",
            json!({"name": "plug.echo", "arguments": {}}),
        );
        let response = wait(&mut daemon, &echo);
        assert_eq!(
            response.result.expect("echo result")["content"][0]["text"],
            "pong"
        );

        let fail = send(&mut daemon, "tools/call", json!({"name": "plug.fail"}));
        let result = wait(&mut daemon, &fail).result.expect("failure result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "boom");

        let hung = send(&mut daemon, "tools/call", json!({"name": "plug.hang"}));
        let _crashed = send(&mut daemon, "tools/call", json!({"name": "plug.crash"}));
        let result = wait(&mut daemon, &hung).result.expect("crash result");
        assert_eq!(result["isError"], true);
        assert!(!daemon.is_live("plug"));

        let tools = daemon.tool_list();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "plug.start");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tool_projection_follows_the_live_state() {
        let registry = registry(vec![plugin("plug", &["echo"])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, "token".into(), receiver, None);

        let tools = daemon.tool_list();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "plug.start");

        daemon.slots.get_mut("plug").expect("slot").machine.state = State::Running;
        let tools = daemon.tool_list();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names, vec!["plug.echo", "plug.start"]);

        daemon.slots.get_mut("plug").expect("slot").machine.state = State::Backoff;
        assert_eq!(daemon.tool_list().len(), 1);
    }

    #[test]
    fn tools_list_and_unknown_tool_use_the_documented_codes() {
        let registry = registry(vec![plugin("plug", &["echo"])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, "token".into(), receiver, None);

        let listed = send(&mut daemon, "tools/list", json!({}));
        let listed = wait(&mut daemon, &listed);
        assert_eq!(
            listed.result.expect("result")["tools"]
                .as_array()
                .expect("array")
                .len(),
            1
        );

        let unknown = send(&mut daemon, "tools/call", json!({"name": "plug.nope"}));
        let unknown = wait(&mut daemon, &unknown);
        assert_eq!(unknown.error.expect("error").code, -32602);

        let missing = send(&mut daemon, "tools/call", json!({}));
        let missing = wait(&mut daemon, &missing);
        assert_eq!(missing.error.expect("error").code, -32602);
    }

    #[test]
    fn a_known_tool_on_a_dead_plugin_is_a_tool_error() {
        let registry = registry(vec![plugin("plug", &["echo"])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, "token".into(), receiver, None);

        let response = send(&mut daemon, "tools/call", json!({"name": "plug.echo"}));
        let response = wait(&mut daemon, &response);
        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("plug.start"));
    }

    #[test]
    fn a_capability_ending_in_start_is_not_shadowed_by_the_lifecycle_tool() {
        let registry = registry(vec![plugin("plug", &["foo.start"])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, "token".into(), receiver, None);

        let response = send(&mut daemon, "tools/call", json!({"name": "plug.foo.start"}));
        let response = wait(&mut daemon, &response);

        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        assert!(result["content"][0]["text"]
            .as_str()
            .expect("text")
            .contains("plug.start"));
    }

    #[test]
    fn core_services_are_not_tools() {
        let registry = registry(vec![service("core", &[])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, "token".into(), receiver, None);

        let response = send(&mut daemon, "tools/call", json!({"name": "core.start"}));
        let response = wait(&mut daemon, &response);
        assert_eq!(response.error.expect("error").code, -32602);
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
