use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

#[cfg(feature = "http")]
use crate::api::{self, Api};
#[cfg(not(feature = "http"))]
type Api = ();
use crate::catalog;
use crate::channel;
use crate::compat;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::definition;
use crate::kernel::contract::{
    CapabilityName, Instance, Invocation, Launchers, ProgressToken, Reply, ReplySink,
};
use crate::kernel::failure::{Class, Failure, Problem, ProblemError, Rejection, Retry};
use crate::lan::LanStatus;
use crate::log;
use crate::mcp;
use crate::package;
use crate::paths;
use crate::plugin::{self, Strategy};
#[cfg(unix)]
use crate::reactor::Reactor;
use crate::registry::{Hook, Kind, Registry, Service, State, RESERVED_ID};
use crate::rpc;
use crate::runtime::{self, Record};
use crate::settings;
use crate::slot::hook_command;
use crate::slot::{Slot, SlotWire};
use crate::status::Status;
use crate::supervisor::{self, Event};
use crate::wake::Wake;

/// The supervisor loop's bounded wait when it has no earlier deadline. With a
/// reactor this is the plugin poll's timeout, so an idle daemon wakes once per
/// tick: the poll doubles as the wait for shutdown and the loop's timers.
const POLL_WAIT: Duration = Duration::from_millis(50);

pub fn run(conf: Option<PathBuf>) -> i32 {
    // The explicit --conf must be loaded before paths::config_dir() so its
    // config_dir wins and does not get shadowed by the default cache.
    if let Some(path) = conf.as_deref() {
        if path.is_file() {
            if let Err(error) = settings::init_from_path(path) {
                eprintln!("svcd: invalid config: {}", error);
                return 1;
            }
        }
    }
    let initial_config = paths::config_dir();
    if let Err(error) = fs::create_dir_all(&initial_config) {
        eprintln!(
            "svcd: cannot create config directory {}: {}",
            initial_config.display(),
            error
        );
        return 1;
    }

    let config_path = match crate::install::ensure_conf(conf.as_deref()) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("svcd: {}", error);
            return 1;
        }
    };
    if let Err(error) = settings::init_from_path(&config_path) {
        eprintln!("svcd: invalid config: {}", error);
        return 1;
    }
    if let Err(error) = log::init_svcd(&paths::logs_dir()) {
        eprintln!("svcd: cannot open log directory: {}", error);
    }
    log::svcd("svcd starting");
    log::svcd(&format!("svcd: config: {}", config_path.display()));
    // An explicit --conf may set config_dir to a different directory.
    let config = paths::config_dir();
    if config != initial_config {
        if let Err(error) = fs::create_dir_all(&config) {
            eprintln!(
                "svcd: cannot create config directory {}: {}",
                config.display(),
                error
            );
            return 1;
        }
    }
    crate::plugin::cleanup_staging();
    // The channel no longer uses a token: authorization is the endpoint ACL.
    // Remove whatever the previous version left behind so nothing keeps reading
    // a stale secret.
    let legacy_token = config.join("svcd.token");
    if legacy_token.exists() {
        let _ = fs::remove_file(&legacy_token);
        log::svcd("svcd: removed the legacy control token (endpoint ACLs authorize now)");
    }
    #[cfg(windows)]
    if crate::platform::host::acl::is_machine_wide(&config) {
        if let Err(error) = harden_directories() {
            eprintln!("svcd: cannot protect the install directories: {}", error);
            return 1;
        }
    }
    if let Err(error) = crate::platform::host::process::init() {
        log::svcd(&format!(
            "svcd: parent-death protection unavailable: {}",
            error
        ));
    }

    wait_for_services_dir(&paths::services_dir());

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

    let (tx, rx) = mpsc::sync_channel(channel::PENDING_QUEUE_LIMIT);
    let wake = Arc::new(Wake::new());
    // One path on every host: the channel binds the endpoint and either hands
    // the listener to the server loop (Unix) or starts its own accept thread
    // (Windows). The composition root only sees the resulting server.
    let control = match channel::bind_server(&config, tx, wake.clone()) {
        Ok(control) => control,
        Err(error) => {
            log::svcd(&format!("svcd: cannot bind control channel: {}", error));
            return 1;
        }
    };
    #[cfg(feature = "http")]
    let raw_hub = crate::rawhub::RawHub::new(
        crate::limits::current().ws_raw_max_bytes,
        crate::limits::current().ws_inflight_bytes,
        crate::limits::current().ws_inflight_requests,
        crate::limits::current().ws_raw_queue,
    );
    // The driver picks itself inside `server::http::entry`; the composition
    // root only hands over the control plumbing it owns.
    #[cfg(feature = "http")]
    let api = match api::spawn(config.clone(), raw_hub.clone(), control) {
        Ok(api) => Some(api),
        Err(error) => {
            #[cfg(unix)]
            {
                // The server loop now owns the control channel; failing to
                // start it is fatal, not an HTTP-only degradation.
                log::svcd(&format!("svcd: cannot start server loop: {}", error));
                return 1;
            }
            #[cfg(not(unix))]
            {
                log::svcd(&format!("svcd: HTTP API disabled: {}", error));
                None
            }
        }
    };
    #[cfg(all(unix, not(feature = "http")))]
    let control_loop = match crate::server::control::spawn_control_only(control) {
        Ok(handle) => handle,
        Err(error) => {
            log::svcd(&format!("svcd: cannot start server loop: {}", error));
            return 1;
        }
    };
    // A host whose control loop already started (Windows) has nothing left to
    // hand over: the accept thread owns the server from `bind_server` on.
    #[cfg(all(not(unix), not(feature = "http")))]
    drop(control);
    #[cfg(not(feature = "http"))]
    let raw_hub = crate::rawhub::RawHub::new(
        crate::limits::current().ws_raw_max_bytes,
        crate::limits::current().ws_inflight_bytes,
        crate::limits::current().ws_inflight_requests,
        crate::limits::current().ws_raw_queue,
    );
    #[cfg(not(feature = "http"))]
    let api: Option<Arc<Api>> = None;
    notify_systemd("READY=1");

    let mut daemon = Daemon::new_with_raw(loaded.registry, rx, api.clone(), raw_hub.clone());
    daemon.set_raw_hub(raw_hub.clone());
    daemon.set_wake(wake);
    #[cfg(unix)]
    {
        let reactor = match Reactor::start() {
            Ok(reactor) => Some(reactor),
            Err(error) => {
                log::svcd(&format!(
                    "svcd: reactor unavailable; using per-plugin reader threads: {}",
                    error
                ));
                None
            }
        };
        if let Some(reactor) = &reactor {
            raw_hub.set_reactor(reactor);
        }
        daemon.set_reactor(reactor);
    }
    daemon.adopt_runtime();
    daemon.sweep_runtime_records();
    install_shutdown_signal();
    daemon.start_enabled();
    daemon.event_loop();
    // The supervisor loop has returned and no reply can arrive after this
    // point. The server loop drains queued control replies before the process
    // exits, so the shutdown response is not lost.
    #[cfg(feature = "http")]
    if let Some(api) = &api {
        api.shutdown();
    }
    #[cfg(all(unix, not(feature = "http")))]
    control_loop.shutdown();
    #[cfg(feature = "http")]
    log::svcd(&format!("svcd: raw stats: {}", raw_hub.stats.summary()));
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

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Name + mtime + size of every entry in `services.d`. Cheap enough to poll;
/// any create/delete/edit changes it and triggers a reconcile.
fn services_fingerprint(directory: &Path) -> Vec<(String, u64, u64)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut fingerprint: Vec<(String, u64, u64)> = entries
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let (mtime, size) = entry
                .metadata()
                .ok()
                .map(|metadata| {
                    let mtime = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos() as u64)
                        .unwrap_or(0);
                    (mtime, metadata.len())
                })
                .unwrap_or((0, 0));
            (name, mtime, size)
        })
        .collect();
    fingerprint.sort();
    fingerprint
}

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The service directory lives on the overlay on read-only-rootfs hosts, so it
/// can appear a few seconds after inittab starts svcd. Wait for it instead of
/// scanning an empty tree once and never looking again; `svc reload` covers the
/// rest. `SVCD_BOOT_SCAN_WAIT_MS=0` disables the wait (tests, desktop).
fn wait_for_services_dir(dir: &Path) {
    let budget: u64 = std::env::var("SVCD_BOOT_SCAN_WAIT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10_000);
    if dir.is_dir() {
        return;
    }
    let started = Instant::now();
    loop {
        if dir.is_dir() {
            log::svcd(&format!(
                "svcd: service directory appeared after {}ms: {}",
                started.elapsed().as_millis(),
                dir.display()
            ));
            return;
        }
        if started.elapsed().as_millis() as u64 >= budget {
            log::svcd(&format!(
                "svcd: service directory still missing after {}ms: {} (use `svc reload`)",
                started.elapsed().as_millis(),
                dir.display()
            ));
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(unix)]
fn install_shutdown_signal() {
    extern "C" fn handle(_: libc::c_int) {
        request_shutdown();
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
        request_shutdown();
        1
    }

    unsafe {
        SetConsoleCtrlHandler(Some(handle), 1);
    }
}

/// Ask the running daemon to shut down. The event loop checks this flag on
/// every wake, so callers on any thread (the Windows SCM control handler, for
/// example) can stop it without touching daemon state.
pub fn request_shutdown() {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

pub fn shutdown() -> i32 {
    shutdown_inner(true)
}

/// Stop without printing "unreachable" when the daemon already exited.
pub fn shutdown_quiet() -> i32 {
    shutdown_inner(false)
}

fn shutdown_inner(verbose: bool) -> i32 {
    let config = paths::config_dir();
    match channel::request(&config, "svcd.shutdown", json!({})) {
        Ok(response) => match response.error {
            Some(error) => {
                if verbose {
                    eprintln!("svcd: {}", error.message);
                }
                1
            }
            None => 0,
        },
        Err(error) if shutdown_reply_lost(&error) => {
            // The request reached a daemon that then exited. On Windows the
            // per-connection control thread can lose the race with process exit
            // and close the pipe before its reply frame is written; the stop
            // hooks have already run. Reporting "unreachable" here would turn a
            // successful stop into a failure.
            0
        }
        Err(error) => {
            if verbose {
                eprintln!("svcd: svcd unreachable: {}", error);
            }
            2
        }
    }
}

/// Whether an error means the daemon closed the control channel after taking
/// the shutdown request, instead of never answering it.
fn shutdown_reply_lost(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}

/// The daemon runs `services.d` scripts and plugin payloads as LocalSystem, so
/// those directories must not be writable by anyone but SYSTEM, Administrators
/// and the `svcd` group. Without this a local user could drop a `.cmd` into
/// `services.d` and have it executed as SYSTEM.
#[cfg(windows)]
fn harden_directories() -> std::io::Result<()> {
    use crate::platform::host::acl::Acl;
    for dir in [paths::config_dir(), paths::services_dir(), paths::plugins_dir()] {
        // Create first: a first start has no `services.d` / `plugins` yet, and a
        // directory created later would inherit `C:\ProgramData`'s Users-write
        // ACE instead of getting the protected DACL below.
        fs::create_dir_all(&dir)?;
        // Group members may read the config and inspect the tree, but they must
        // not be able to replace a `services.d` script or plugin payload that
        // this daemon executes as SYSTEM.
        Acl::shared_read_only(true)?.apply_to_path(&dir)?;
    }
    Ok(())
}

struct Daemon {
    registry: Registry,
    rx: Receiver<channel::PendingCall>,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    api: Option<Arc<Api>>,
    wake: Arc<Wake>,
    slots: BTreeMap<String, Slot>,
    /// The tag -> runtime factory assembly point. A launch is a cheap view;
    /// the Slot (or the builtin service) keeps owning the resource.
    launchers: Arc<Launchers>,
    shutdown: bool,
    started: Instant,
    epoch_at_start: SystemTime,
    catalog_version: u64,
    catalog_cache: Option<(u64, Vec<Value>)>,
    /// Last record written per service: state, pid, failures. Only a change
    /// here triggers a disk write.
    persisted: BTreeMap<String, (State, Option<u32>, u32)>,
    /// Boot identity captured once; every runtime record carries it.
    boot_id: Option<String>,
    /// Cheap fingerprint of `services.d`; a change triggers a reconcile.
    services_fingerprint: Vec<(String, u64, u64)>,
    rescan_interval_ms: u64,
    next_rescan_ms: u64,
    /// Temp-file retention and the supervisor's own cleanup cadence. The
    /// supervisor loop owns timers, so there is no standalone cleanup thread.
    temp_retention: Duration,
    temp_cleanup_interval_ms: u64,
    next_temp_cleanup_ms: u64,
    #[cfg(unix)]
    reactor: Option<Arc<Reactor>>,
    /// Who reads plugin stdio: copied into every slot, including ones created
    /// after the poll set started.
    stdio: crate::stdio::Target,
    raw_hub: Arc<crate::rawhub::RawHub>,
}

impl Daemon {
    #[cfg(test)]
    fn new(
        registry: Registry,
        rx: Receiver<channel::PendingCall>,
        api: Option<Arc<Api>>,
    ) -> Self {
        let limits = crate::limits::current();
        Self::new_with_raw(
            registry,
            rx,
            api,
            crate::rawhub::RawHub::new(
                limits.ws_raw_max_bytes,
                limits.ws_inflight_bytes,
                limits.ws_inflight_requests,
                limits.ws_raw_queue,
            ),
        )
    }

    fn new_with_raw(
        registry: Registry,
        rx: Receiver<channel::PendingCall>,
        api: Option<Arc<Api>>,
        raw_hub: Arc<crate::rawhub::RawHub>,
    ) -> Self {
        let wake = Arc::new(Wake::new());
        let rescan_interval_ms: u64 = std::env::var("SVCD_RESCAN_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5_000);
        let mut slots = BTreeMap::new();
        for service in registry.managed_values() {
            if let Some(exec) = service.exec.clone() {
                let mut slot = Slot::new(
                    service.id.clone(),
                    exec,
                    service.lifecycle.clone(),
                    service.restart,
                );
                slot.set_protocol(service.protocol);
                slot.set_protocol_only(service.kind == Kind::Plugin);
                slot.set_remain_after_exit(service.remain_after_exit);
                slot.set_initial_state(service.state);
                slot.set_wake(wake.clone());
                configure_slot(&mut slot, &service);
                slots.insert(service.id.clone(), slot);
            }
        }
        let (temp_retention, temp_cleanup_interval_ms) = match settings::current() {
            Ok(settings) => (
                settings.temp_retention(),
                settings.temp_cleanup_interval().as_millis() as u64,
            ),
            Err(_) => (crate::settings::Settings::default().temp_retention(), 0),
        };
        let mut launchers = Launchers::default();
        launchers.register("builtin", Box::new(crate::builtin_adapter::BuiltinLauncher));
        Daemon {
            registry,
            rx,
            api,
            wake,
            slots,
            launchers: Arc::new(launchers),
            shutdown: false,
            started: Instant::now(),
            epoch_at_start: SystemTime::now(),
            catalog_version: 1,
            catalog_cache: None,
            persisted: BTreeMap::new(),
            boot_id: crate::platform::host::process::boot_id(),
            services_fingerprint: services_fingerprint(&paths::services_dir()),
            rescan_interval_ms,
            next_rescan_ms: rescan_interval_ms,
            temp_retention,
            temp_cleanup_interval_ms,
            // The old cleanup thread ran once before its first sleep; the
            // supervisor does the same by treating the first tick as due.
            next_temp_cleanup_ms: 0,
            #[cfg(unix)]
            reactor: None,
            stdio: crate::stdio::Target::threaded(),
            raw_hub,
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Convert a monotonic supervisor timestamp to epoch seconds.
    fn epoch_of(&self, ms: u64) -> u64 {
        let now = self.now_ms();
        let now_epoch = now_epoch_secs();
        now_epoch.saturating_sub(now.saturating_sub(ms) / 1000)
    }

    /// The inverse of [`epoch_of`]: epoch seconds back to a monotonic stamp.
    fn ms_of(&self, epoch: u64) -> u64 {
        let now = self.now_ms();
        let now_epoch = now_epoch_secs();
        now.saturating_sub(now_epoch.saturating_sub(epoch).saturating_mul(1000))
    }

    /// Reconnect to processes that survived a previous daemon. A record is only
    /// trusted when its boot id matches, the pid is alive and the pid's start
    /// time still matches (a reused pid fails this check). On Windows the job
    /// objects kill the tree with the daemon, so there is nothing to adopt.
    fn adopt_runtime(&mut self) {
        let boot = self.boot_id.clone().or_else(crate::platform::host::process::boot_id);
        let now = self.now_ms();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            let Some(record) = runtime::load(&id) else {
                continue;
            };
            if !runtime::same_boot(&record, boot.as_deref()) {
                log::svcd(&format!(
                    "svcd: {}: discarding runtime record from another boot",
                    id
                ));
                runtime::remove(&id);
                continue;
            }
            let Some(pid) = record.pid else {
                // A finished one-shot has no pid but its effect survives: keep
                // reporting running instead of re-running the work on restart.
                // This works on every platform because no process is involved.
                let keep = record.state == State::Running
                    && self
                        .slots
                        .get(&id)
                        .map(|slot| slot.machine.remain_after_exit)
                        .unwrap_or(false);
                if keep {
                    let since = self.ms_of(record.since.unwrap_or_else(now_epoch_secs));
                    let last_exit = record.last_exit;
                    let failures = record.failures;
                    if let Some(slot) = self.slots.get_mut(&id) {
                        slot.adopt_finished(since, last_exit);
                    }
                    self.persisted
                        .insert(id.clone(), (State::Running, None, failures));
                } else {
                    runtime::remove(&id);
                }
                continue;
            };
            // Windows has no process to adopt: the job object took the tree
            // down with the daemon. Records are only cleaned up there.
            if cfg!(windows) {
                runtime::remove(&id);
                continue;
            }
            if !crate::platform::host::process::alive(pid) {
                runtime::remove(&id);
                continue;
            }
            if let (Some(expected), Some(actual)) = (record.starttime, crate::platform::host::process::start_time(pid))
            {
                if expected != actual {
                    log::svcd(&format!(
                        "svcd: {}: pid {} was reused (starttime {} != {}); not adopting",
                        id, pid, actual, expected
                    ));
                    runtime::remove(&id);
                    continue;
                }
            }
            let since = self.ms_of(record.since.unwrap_or_else(now_epoch_secs));
            let ready = record.ready_since.map(|epoch| self.ms_of(epoch));
            let failures = record.failures;
            let last_exit = record.last_exit;
            let state = record.state;
            let was_stopping = state == State::Stopping;
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.adopt(pid, state, since, ready, failures, last_exit, now);
                if was_stopping {
                    // Finish the interrupted stop the same way a live stop
                    // does: stop action first, then signals and the kill
                    // window. Otherwise the service's cleanup would be lost.
                    slot.request_stop(now);
                }
                let adopted = slot.machine.state;
                let adopted_pid = slot.machine.pid;
                self.persisted
                    .insert(id.clone(), (adopted, adopted_pid, failures));
            }
        }
    }

    /// Remove runtime records that no longer have a definition. `reload` and
    /// the daemon's own removal paths clean records for services they know
    /// about; a definition deleted while the daemon was down leaves an orphan
    /// that nothing else would ever visit.
    fn sweep_runtime_records(&mut self) {
        let directory = paths::run_dir();
        let Ok(entries) = fs::read_dir(&directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if self.registry.get(id).is_none() {
                // A definition deleted while the daemon was down: take its
                // process group down too (validated by pid + starttime), then
                // forget the record.
                if let Some(record) = runtime::load(id) {
                    if let Some(pid) = record.pid {
                        let fresh = crate::platform::host::process::alive(pid)
                            && match (record.starttime, crate::platform::host::process::start_time(pid)) {
                                (Some(expected), Some(actual)) => expected == actual,
                                _ => true,
                            };
                        if fresh {
                            crate::platform::host::process::terminate_tree(pid);
                        }
                    }
                }
                runtime::remove(id);
                self.persisted.remove(id);
                log::svcd(&format!("svcd: dropped orphan runtime record {}", id));
            }
        }
    }

    /// Write the runtime record of every service whose (state, pid, failures)
    /// tuple changed since the last write. Stopped/failed services with no pid
    /// need no record: there is nothing to reconnect.
    fn persist_states(&mut self) {
        let boot = self.boot_id.clone();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            let (state, pid, failures) = {
                let machine = &self.slots[&id].machine;
                (machine.state, machine.pid, machine.failures)
            };
            if self.persisted.get(&id) == Some(&(state, pid, failures)) {
                continue;
            }
            if pid.is_none() && matches!(state, State::Stopped | State::Failed) {
                runtime::remove(&id);
                self.persisted.remove(&id);
                continue;
            }
            let (since, ready_since, last_exit) = {
                let machine = &self.slots[&id].machine;
                (
                    self.epoch_of(machine.since_ms),
                    machine.ready_since_ms.map(|ms| self.epoch_of(ms)),
                    machine.last_exit,
                )
            };
            let record = Record {
                boot: boot.clone(),
                state,
                pid,
                starttime: pid.and_then(crate::platform::host::process::start_time),
                since: Some(since),
                ready_since,
                failures,
                last_exit,
            };
            match runtime::save(&id, &record) {
                Ok(()) => {
                    self.persisted.insert(id, (state, pid, failures));
                }
                Err(error) => log::svcd(&format!("svcd: {}: cannot persist state: {}", id, error)),
            }
        }
    }

    fn set_wake(&mut self, wake: Arc<Wake>) {
        self.wake = wake.clone();
        for slot in self.slots.values_mut() {
            slot.set_wake(wake.clone());
        }
    }

    #[cfg(unix)]
    fn set_reactor(&mut self, reactor: Option<Arc<Reactor>>) {
        // The supervisor loop polls plugin stdio itself. Attaching the poll's
        // self-pipe to the shared wake latch means a management request or a
        // queued raw frame interrupts that poll instead of waiting it out.
        if let Some(reactor) = &reactor {
            // Synchronous waits on this thread (an install hook, a status
            // hook) poll through this binding, so plugin pipes keep moving.
            crate::reactor::bind_waiting_loop(reactor);
            match reactor.interrupt_handle() {
                Ok(interrupt) => self.wake.set_interrupt(interrupt),
                // Without the self-pipe the loop still polls, so requests are
                // served; they just wait for the poll timeout instead of
                // interrupting it.
                Err(error) => log::svcd(&format!(
                    "svcd: cannot bind the plugin-stdio poll wake; requests wait for the poll timeout: {}",
                    error
                )),
            }
        }
        self.reactor = reactor.clone();
        self.stdio = match reactor {
            Some(reactor) => crate::stdio::Target::polled(reactor),
            None => crate::stdio::Target::threaded(),
        };
        for slot in self.slots.values_mut() {
            slot.set_stdio(self.stdio.clone());
        }
    }

    fn set_raw_hub(&mut self, raw_hub: Arc<crate::rawhub::RawHub>) {
        self.raw_hub = raw_hub;
        for slot in self.slots.values_mut() {
            slot.set_raw_hub(Some(self.raw_hub.clone()));
        }
    }

    fn start_enabled(&mut self) {
        let mut eligible = Vec::new();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            let enabled = self
                .registry
                .get(&id)
                .map(|service| service.enabled)
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            // A reload must not resurrect services the operator stopped or
            // bring a Failed service back behind their back.
            if self.slots[&id].machine.state != State::Stopped {
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
            let mut disconnected = false;
            loop {
                match self.rx.try_recv() {
                    Ok(incoming) => self.handle(incoming),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            self.pump();
            if self.rescan_interval_ms > 0 && self.now_ms() >= self.next_rescan_ms {
                self.next_rescan_ms = self.now_ms().saturating_add(self.rescan_interval_ms);
                self.rescan_if_changed();
            }
            if self.temp_cleanup_interval_ms > 0 && self.now_ms() >= self.next_temp_cleanup_ms {
                self.next_temp_cleanup_ms = self
                    .now_ms()
                    .saturating_add(self.temp_cleanup_interval_ms);
                match crate::blob::cleanup(self.temp_retention) {
                    Ok(0) => {}
                    Ok(removed) => log::svcd(&format!(
                        "svcd: temp cleanup removed {} file(s)",
                        removed
                    )),
                    Err(error) => log::svcd(&format!("svcd: temp cleanup failed: {}", error)),
                }
            }
            if SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
                self.shutdown = true;
            }
            if self.shutdown || disconnected {
                break;
            }
            // Management requests and plugin stdout wake this immediately;
            // the bounded wait is only for shutdown and timer deadlines.
            self.wait_for_work(POLL_WAIT);
        }
        self.stop_all();
    }

    /// Bounded wait for the next event. With a reactor this wait *is* the
    /// plugin poll: the supervisor loop owns plugin stdio, and the shared wake
    /// latch writes to the poll's self-pipe so a management request returns at
    /// once. Without one (Windows, or a reactor that failed to start) the
    /// condvar latch is the wake source.
    fn wait_for_work(&self, timeout: Duration) {
        #[cfg(unix)]
        if let Some(reactor) = &self.reactor {
            reactor.poll_once(timeout);
            return;
        }
        self.wake.wait_timeout(timeout);
    }

    fn handle(&mut self, incoming: channel::PendingCall) {
        let request = match rpc::parse(&incoming.line) {
            Ok(request) => request,
            Err(response) => {
                let _ = incoming.reply.send(rpc::to_line(&response));
                return;
            }
        };
        if incoming.endpoint == channel::Endpoint::User
            && channel::Endpoint::for_method(&request.method) == channel::Endpoint::Admin
        {
            let response = rpc::err_data(
                request.id,
                -32000,
                "unauthorized",
                problem_data(Problem::Internal),
            );
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
        let id = request.id.clone();
        match request.method.as_str() {
            "svcd.ping" => rpc::ok(id, json!({"ok": true})),
            "svcd.compat" => rpc::ok(
                id,
                serde_json::to_value(compat::detect(MAX_SCHEMA, MAX_PROTOCOL)).unwrap_or(Value::Null),
            ),
            "svcd.shutdown" => {
                self.shutdown = true;
                rpc::ok(id, json!({"ok": true}))
            }
            "registry.tools" | "tools/list" => match self.tools_page(&request.params) {
                Ok(value) => rpc::ok(id, value),
                Err(error) => problem_response(id, -32602, error),
            },
            "registry.version" => rpc::ok(id, json!({ "version": self.catalog_version })),
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
                    None => rpc::err_data(
                        id,
                        -32001,
                        &format!("unknown service: {}", service_id),
                        problem_data(Problem::UnknownService),
                    ),
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
            "svc.restart" => match param_id(&request) {
                Ok(service_id) => status_response(id, -32010, self.restart_service(&service_id)),
                Err(response) => response,
            },
            "svc.reload" | "svcd.reload" => match self.reload() {
                Ok(summary) => rpc::ok(id, summary),
                Err(error) => problem_response(id, -32009, error),
            },
            "plugin.install" => match self.install_plugin(&request) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => problem_response(id, -32006, error),
            },
            "plugin.install_staged" => match self.install_staged(&request) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => problem_response(id, -32006, error),
            },
            "plugin.uninstall" => match self.uninstall_plugin(&request) {
                Ok(()) => rpc::ok(id, json!({ "ok": true })),
                Err(error) => problem_response(id, -32007, error),
            },
            "auth.status" => match self.auth_status() {
                Ok(status) => rpc::ok(id, auth_json(&status)),
                Err(error) => problem_response(id, -32008, error),
            },
            "auth.passwd" => {
                let password = request
                    .params
                    .get("password")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self.auth_passwd(password) {
                    Ok(status) => rpc::ok(id, auth_json(&status)),
                    Err(error) => problem_response(id, -32008, error),
                }
            }
            _ => rpc::err_data(
                id,
                -32601,
                "method not found",
                problem_data(Problem::Invalid),
            ),
        }
    }

    #[cfg(feature = "http")]
    fn auth_status(&self) -> Result<LanStatus, ProblemError> {
        self.api
            .as_ref()
            .ok_or_else(|| ProblemError::new(Problem::Internal, "HTTP API is not running"))?
            .status()
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))
    }

    #[cfg(not(feature = "http"))]
    fn auth_status(&self) -> Result<LanStatus, ProblemError> {
        Err(ProblemError::new(
            Problem::Internal,
            "HTTP API is disabled in this build",
        ))
    }

    #[cfg(feature = "http")]
    fn auth_passwd(&self, password: &str) -> Result<LanStatus, ProblemError> {
        if password.is_empty() {
            return Err(ProblemError::new(
                Problem::Internal,
                "password must not be empty",
            ));
        }
        let path = settings::path().ok_or_else(|| {
            ProblemError::new(Problem::Internal, "svcd.conf path is unavailable")
        })?;
        let hash = crate::auth::hash_password(password)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        settings::update_value(path, "auth_passwd", &hash)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        self.api
            .as_ref()
            .ok_or_else(|| ProblemError::new(Problem::Internal, "HTTP API is not running"))?
            .set_password(hash)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))
    }

    #[cfg(not(feature = "http"))]
    fn auth_passwd(&self, _password: &str) -> Result<LanStatus, ProblemError> {
        Err(ProblemError::new(
            Problem::Internal,
            "HTTP API is disabled in this build",
        ))
    }

    fn statuses(&self) -> Vec<Status> {
        self.registry
            .managed_values()
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
        if let Some(service) = self.registry.get(id) {
            if service
                .impl_ref
                .as_ref()
                .map(|spec| spec.tag.as_str() == "builtin")
                .unwrap_or(false)
            {
                // The builtin `svc` has no slot; its registry state is the truth.
                return service.state == State::Running;
            }
        }
        self.slots
            .get(id)
            .map(|slot| slot.machine.state == State::Running)
            .unwrap_or(false)
    }

    fn tool_list(&self) -> Vec<Value> {
        mcp::tools(&self.registry, |id| self.is_live(id))
    }

    /// Invalidate the cached tool projection. The cache is keyed by the
    /// catalog version so a page is always cut from one stable snapshot.
    fn bump_catalog(&mut self) {
        self.catalog_version = self.catalog_version.wrapping_add(1);
        self.catalog_cache = None;
    }

    fn tool_list_cached(&mut self) -> &[Value] {
        let fresh = self
            .catalog_cache
            .as_ref()
            .map(|(version, _)| *version == self.catalog_version)
            .unwrap_or(false);
        if !fresh {
            let tools = self.tool_list();
            self.catalog_cache = Some((self.catalog_version, tools));
        }
        &self
            .catalog_cache
            .as_ref()
            .expect("catalog cache was just populated")
            .1
    }

    fn tools_page(&mut self, params: &Value) -> Result<Value, ProblemError> {
        let cursor = params.get("cursor").and_then(Value::as_str);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value.min(usize::MAX as u64) as usize);
        let version = self.catalog_version;
        let page = catalog::page(self.tool_list_cached(), version, cursor, limit)
            .map_err(|error| {
                let problem = match error {
                    crate::catalog::PageError::InvalidCursor => Problem::Invalid,
                    // Both today fall through to 500; keep that projection.
                    crate::catalog::PageError::VersionMismatch
                    | crate::catalog::PageError::PastEnd => Problem::Internal,
                };
                ProblemError::new(problem, error.to_string())
            })?;
        let mut result = json!({ "tools": page.tools });
        if let Some(cursor) = page.next_cursor {
            result["nextCursor"] = json!(cursor);
        }
        Ok(result)
    }

    /// Handle tools/call. Plugin calls are deferred: the plugin response arrives
    /// on the slot pump, so the control connection stays open without blocking
    /// the daemon's event loop. A start tool is synchronous because starting is
    /// defined as synchronous in the service contract.
    fn handle_tool_call(&mut self, request: rpc::Request, reply: crate::channel::Reply) {
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
                .map(|service| service.kind == Kind::Plugin || !service.contract.surface().is_empty())
                .unwrap_or(false)
        });
        if let Some(service_id) = start_id {
            let response = match self.start_service(service_id) {
                Ok(status) => mcp::tool_result(id, status_json(status).to_string(), false),
                Err(error) => mcp::tool_error(id, error.detail()),
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
        // A service is callable when its contract declares the capability and
        // the assembly point knows how to reach it. `svc` is builtin and has no
        // supervised process; everything else keeps the SlotWire path.
        let builtin = service.impl_ref.clone();
        let known = service
            .contract
            .capability(&CapabilityName::new(capability))
            .is_some();
        let callable = builtin.is_some()
            || service.kind == Kind::Plugin
            || self.slots.contains_key(service_id);
        if !known || !callable {
            send(&reply, unknown_tool(&id, &name));
            return;
        }
        if builtin.is_some() && !self.is_live(service_id) {
            send(
                &reply,
                mcp::tool_result(id, format!("service {} is not running", service_id), true),
            );
            return;
        }

        let mut arguments = arguments;
        if let Err(message) = crate::blob::resolve_args(&mut arguments) {
            send(
                &reply,
                mcp::tool_error(id, &format!("cannot resolve blob argument: {}", message)),
            );
            return;
        }
        let progress_token = request
            .params
            .get("progressToken")
            .and_then(|token| match token {
                Value::String(token) => Some(ProgressToken::new(token.clone())),
                Value::Number(token) => Some(ProgressToken::new(token.to_string())),
                _ => None,
            });
        let mut invocation = Invocation::new(
            capability,
            arguments,
            crate::limits::current().default_timeout,
        );
        invocation.progress = progress_token;

        if let Some(spec) = builtin {
            let launchers = self.launchers.clone();
            let contract = service.contract.clone();
            let mut instance = match launchers.launch(contract, &spec) {
                Ok(instance) => instance,
                Err(error) => {
                    reply.send(project_builtin_outcome(id, Err(error.into_failure())));
                    return;
                }
            };
            let done = projected_done(
                reply.clone(),
                id.clone(),
                "stream replies are not supported by the builtin adapter",
                project_builtin_outcome,
            );
            if let Err(rejection) = instance.call(invocation, done) {
                reply.send(rpc::to_line(&project_rejection(id, rejection)));
            }
            return;
        }

        let Some(slot) = self.slots.get_mut(service_id) else {
            send(
                &reply,
                mcp::tool_result(id, format!("service {} has no runtime", service_id), true),
            );
            return;
        };
        let handle = slot.handle();
        let contract = service.contract.clone();
        let mut instance = match Instance::new(
            contract,
            Box::new(SlotWire::new(handle, Some(reply.clone()))),
        ) {
            Ok(instance) => instance,
            Err(error) => {
                let failure = Failure::new(Class::ImplementationError, Retry::Never, error);
                reply.send(project_slot_outcome(id, Err(failure)));
                return;
            }
        };
        let done = projected_done(
            reply.clone(),
            id.clone(),
            "stream replies are not wired through SlotWire",
            project_slot_outcome,
        );
        if let Err(rejection) = instance.call(invocation, done) {
            reply.send(rpc::to_line(&project_rejection(id, rejection)));
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
        status.details.probe = Some(run_status_hook(id, &hook, &mut crate::hook::poll_this_loop));
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

    /// Re-scan `services.d` plus the plugin store and reconcile the registry.
    /// Running services keep running (a changed exec applies at the next start);
    /// definitions that disappeared are stopped and forgotten. This is the
    /// recovery path for a late overlay mount, and the operator's way to pick
    /// up a hand-edited definition without restarting the daemon.
    /// Reconcile when `services.d` changed since the last scan: a definition
    /// dropped in while the daemon runs starts (if enabled), a deleted one is
    /// stopped and forgotten. This mirrors the shell `svcd` poll loop.
    fn rescan_if_changed(&mut self) {
        let fingerprint = services_fingerprint(&paths::services_dir());
        if fingerprint == self.services_fingerprint {
            return;
        }
        self.services_fingerprint = fingerprint;
        match self.reconcile_definitions() {
            Ok(summary) => {
                let changed = ["added", "removed", "updated"].iter().any(|key| {
                    summary
                        .get(key)
                        .and_then(Value::as_array)
                        .map(|items| !items.is_empty())
                        .unwrap_or(false)
                });
                if changed {
                    log::svcd(&format!("svcd: rescan: {}", summary));
                }
            }
            Err(error) => log::svcd(&format!("svcd: rescan failed: {}", error)),
        }
    }

    fn reload(&mut self) -> Result<Value, ProblemError> {
        let summary = self.reconcile_definitions()?;
        self.services_fingerprint = services_fingerprint(&paths::services_dir());
        Ok(summary)
    }

    /// Re-scan `services.d` plus the plugin store and reconcile the registry.
    /// Running services keep running (a changed exec applies at the next start);
    /// definitions that disappeared are stopped and forgotten. Shared by
    /// `svc reload`, the periodic rescan, and boot recovery.
    fn reconcile_definitions(&mut self) -> Result<Value, ProblemError> {
        let loaded = definition::load_services(&paths::services_dir());
        for issue in &loaded.issues {
            log::svcd(&format!("svcd: reload: warning: {}", issue));
        }
        let mut desired: BTreeMap<String, Service> = loaded
            .registry
            .managed_values()
            .cloned()
            .map(|service| (service.id.clone(), service))
            .collect();
        let (plugins, warnings) = plugin::scan();
        for warning in warnings {
            log::svcd(&format!("svcd: reload: plugin: {}", warning));
        }
        for service in plugins {
            // A core definition with the same id wins. An installed plugin is
            // authoritative in the registry (install/uninstall update it), but
            // it must still land in `desired`: otherwise the stale sweep below
            // unregisters every installed plugin on each reload. The update
            // branch in `apply_definitions` already skips plugin-over-plugin
            // overwrites, so carrying the scanned snapshot is safe.
            if desired.contains_key(&service.id) {
                continue;
            }
            desired.insert(service.id.clone(), service);
        }

        let mut removed = Vec::new();
        let stale: Vec<String> = self
            .registry
            .managed_values()
            .map(|service| service.id.clone())
            .filter(|id| !desired.contains_key(id))
            .collect();
        for id in stale {
            let active = self
                .slots
                .get(&id)
                .map(|slot| {
                    matches!(
                        slot.machine.state,
                        State::Starting | State::Running | State::Stopping | State::Backoff
                    )
                })
                .unwrap_or(false);
            if active {
                let _ = self.stop_service(&id);
            }
            self.slots.remove(&id);
            self.registry.remove(&id);
            runtime::remove(&id);
            self.persisted.remove(&id);
            log::svcd(&format!("svcd: reload: removed {}", id));
            removed.push(id);
        }

        let (added, updated) = self.apply_definitions(desired);

        // Only definitions that appeared *now* are started: a reload must not
        // resurrect services the operator stopped, nor retry Failed ones.
        for id in &added {
            let enabled = self
                .registry
                .get(id)
                .map(|service| service.enabled)
                .unwrap_or(false);
            if enabled {
                if let Err(error) = self.start_service(id) {
                    log::svcd(&format!("svcd: reload: cannot start {}: {}", id, error));
                }
            }
        }
        self.sweep_runtime_records();
        self.bump_catalog();
        Ok(json!({ "added": added, "removed": removed, "updated": updated }))
    }

    /// One reconcile pass over the desired definitions: register new services and
    /// update existing ones. A service rejected by the registry (ADR-0015's
    /// carrier invariant) is logged and skipped; the live service and its slot
    /// stay exactly as they were.
    fn apply_definitions(
        &mut self,
        desired: BTreeMap<String, Service>,
    ) -> (Vec<String>, Vec<String>) {
        let mut added = Vec::new();
        let mut updated = Vec::new();
        for (id, service) in desired {
            match self.registry.get(&id).cloned() {
                None => {
                    let exec = service.exec.clone();
                    let lifecycle = service.lifecycle.clone();
                    let restart = service.restart;
                    let protocol = service.protocol;
                    let plugin_only = service.kind == Kind::Plugin;
                    let remain_after_exit = service.remain_after_exit;
                    if let Some(exec) = exec {
                        let mut slot = Slot::new(id.clone(), exec, lifecycle, restart);
                        slot.set_protocol(protocol);
                        slot.set_protocol_only(plugin_only);
                        slot.set_remain_after_exit(remain_after_exit);
                        slot.set_wake(self.wake.clone());
                        slot.set_stdio(self.stdio.clone());
                        slot.set_raw_hub(Some(self.raw_hub.clone()));
                        configure_slot(&mut slot, &service);
                        self.slots.insert(id.clone(), slot);
                    }
                    // A rejected service must not leave a slot with no registry
                    // entry: undo the cheap insert.
                    if let Err(reason) = self.registry.insert(service) {
                        self.slots.remove(&id);
                        log::svcd(&format!("svcd: reload: rejecting {}: {}", id, reason));
                        continue;
                    }
                    log::svcd(&format!("svcd: reload: added {}", id));
                    added.push(id);
                }
                Some(old) => {
                    // Plugins are managed through install/uninstall RPCs; scanning
                    // their store must not overwrite live install state.
                    if old.kind == Kind::Plugin && service.kind == Kind::Plugin {
                        continue;
                    }
                    let mut next = service;
                    next.state = old.state;
                    next.pid = old.pid;
                    next.since = old.since;
                    next.ready_since = old.ready_since;
                    next.next_at = old.next_at;
                    next.failures = old.failures;
                    next.last_exit = old.last_exit;
                    let exec = next.exec.clone();
                    let lifecycle = next.lifecycle.clone();
                    let restart = next.restart;
                    let remain_after_exit = next.remain_after_exit;
                    let protocol = next.protocol;
                    // Go through the registry door so the carrier invariant is
                    // enforced here too; a rejected definition keeps the live
                    // service and its slot untouched.
                    if let Err(reason) = self.registry.replace(next) {
                        log::svcd(&format!("svcd: reload: rejecting {}: {}", id, reason));
                        continue;
                    }
                    if let Some(slot) = self.slots.get_mut(&id) {
                        if let Some(exec) = exec {
                            slot.set_exec(exec);
                        }
                        slot.set_lifecycle(lifecycle);
                        slot.set_restart(restart);
                        slot.set_remain_after_exit(remain_after_exit);
                        slot.set_protocol(protocol);
                    }
                    log::svcd(&format!("svcd: reload: updated {}", id));
                    updated.push(id);
                }
            }
        }
        (added, updated)
    }

    fn start_service(&mut self, id: &str) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id).expect("builtin"));
        }
        // A restricted package must never start on a host it does not support,
        // even if the marker changed after it was installed.
        if let Some(service) = self.registry.get(id) {
            let devices = service
                .compat
                .as_ref()
                .map(|spec| spec.devices.clone())
                .unwrap_or_default();
            if let Some(reason) = compat::verdict_reason(compat::evaluate(
                crate::platform::target(),
                &devices,
                &compat::detect(MAX_SCHEMA, MAX_PROTOCOL),
            )) {
                return Err(Problem::Internal.error(format!("{}: {}", id, reason)));
            }
        }
        if let Some(service) = self.registry.get(id) {
            let exec = service.exec.clone();
            let lifecycle = service.lifecycle.clone();
            let restart = service.restart;
            let remain_after_exit = service.remain_after_exit;
            if let Some(slot) = self.slots.get_mut(id) {
                if let Some(exec) = exec {
                    slot.set_exec(exec);
                }
                slot.set_lifecycle(lifecycle);
                slot.set_restart(restart);
                slot.set_remain_after_exit(remain_after_exit);
            }
        }

        // A finished one-shot is `Running` with no pid. An explicit start must
        // run it again, while a dependency in the same state stays satisfied
        // (start_layer skips Running services).
        let now = self.now_ms();
        if let Some(slot) = self.slots.get_mut(id) {
            if slot.machine.state == State::Running
                && slot.machine.pid.is_none()
                && slot.machine.remain_after_exit
            {
                slot.apply(Event::Start, now);
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
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("dependency {} is failed", dependency),
                    ));
                }
                Some(State::Backoff) => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("dependency {} is backing off", dependency),
                    ));
                }
                Some(State::Stopping) => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("dependency {} is stopping", dependency),
                    ));
                }
                _ => {}
            }
        }
        let deadline = self.now_ms().saturating_add(supervisor::START_TIMEOUT_MS);
        for layer in layers {
            self.start_layer(&layer, deadline)?;
        }

        self.status_for_id(id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    fn start_layer(&mut self, ids: &[String], deadline: u64) -> Result<(), ProblemError> {
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
                return Err(ProblemError::new(
                    Problem::Internal,
                    format!("service {} is stopping", id),
                ));
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
                        return Err(ProblemError::new(
                            Problem::Internal,
                            format!("service {} failed to start", id),
                        ));
                    }
                    _ => all_running = false,
                }
            }
            if all_running {
                return Ok(());
            }
            if self.now_ms() >= deadline {
                return Err(ProblemError::new(
                    Problem::Timeout,
                    format!("start timed out: {}", pending.join(", ")),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn install_plugin(&mut self, request: &rpc::Request) -> Result<Status, ProblemError> {
        let path = request
            .params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProblemError::new(Problem::Internal, "plugin.install requires path")
            })?;
        let strategy = match Strategy::parse(
            request
                .params
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("replace"),
        ) {
            Some(strategy) => strategy,
            None => {
                return Err(ProblemError::new(
                    Problem::UnknownStrategy,
                    "unknown strategy",
                ))
            }
        };
        let allow_hooks = request
            .params
            .get("allowHooks")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Everything that can reject the package runs before anything is
        // stopped or written: a bad package must leave the installed version
        // untouched. `plugin::verify` is the same preflight `plugin verify`
        // exposes, so the two answers cannot drift apart.
        let verified = plugin::verify(
            Path::new(path),
            settings::allow_insecure_install(),
            allow_hooks,
        )?;
        let plugin::Verified {
            package,
            signature_verified,
            interpreter: host_interpreter,
        } = verified;
        let id = package.manifest.name.clone();
        if let Some(existing) = self.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::CoreService,
                    format!("id is a core service: {}", id),
                ));
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
        let service = match plugin::install_package(
            &package,
            strategy,
            allow_hooks,
            host_interpreter.clone(),
        ) {
            Ok(service) => service,
            Err(plugin::InstallFailure::OutOfSpace(error)) if strategy == Strategy::KeepOld => {
                if still_running {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!(
                            "not enough space for keep-old; stop the plugin and retry with replace: {}",
                            error
                        ),
                    ));
                }
                log::svcd(&format!(
                    "svcd: plugin {}: keep-old ran out of space; falling back to replace",
                    id
                ));
                plugin::install_package(&package, Strategy::Replace, allow_hooks, host_interpreter)
                    .map_err(plugin::InstallFailure::into_problem)?
            }
            Err(failure) => return Err(failure.into_problem()),
        };
        // The package is already on disk, but registration can still be refused
        // (ADR-0015's carrier invariant). Check before touching the running slot
        // so a refusal leaves the live service untouched.
        if let Err(error) = service.carrier() {
            return Err(ProblemError::new(
                Problem::Internal,
                format!("plugin {}: {}", id, error),
            ));
        }
        if !still_running {
            self.slots.remove(&id);
            if let Some(exec) = service.exec.clone() {
                let mut slot =
                    Slot::new(id.clone(), exec, service.lifecycle.clone(), service.restart);
                slot.set_protocol(service.protocol);
                slot.set_protocol_only(true);
                slot.set_remain_after_exit(service.remain_after_exit);
                slot.set_wake(self.wake.clone());
                slot.set_stdio(self.stdio.clone());
                slot.set_raw_hub(Some(self.raw_hub.clone()));
                // A slot created by a runtime install needs the same contract,
                // projector, call budget and notification sink the boot path
                // gives it. Without this the plugin answers calls but its
                // notifications never reach the UI bus, and default_timeout is
                // not applied to its calls.
                configure_slot(&mut slot, &service);
                self.slots.insert(id.clone(), slot);
            }
        }
        self.registry
            .replace(service)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        self.bump_catalog();
        log::svcd(&format!(
            "svcd: plugin {} {} installed ({}){}",
            id,
            package.manifest.version,
            if still_running {
                "running; next start uses"
            } else {
                "now"
            },
            if signature_verified {
                ""
            } else {
                "; signature was NOT verified (allow_insecure_install)"
            }
        ));
        self.status_for_id(&id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    fn install_staged(&mut self, request: &rpc::Request) -> Result<Status, ProblemError> {
        let path = request
            .params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProblemError::new(Problem::Internal, "plugin.install_staged requires path")
            })?;
        let strategy = match Strategy::parse(
            request
                .params
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("replace"),
        ) {
            Some(strategy) => strategy,
            None => {
                return Err(ProblemError::new(
                    Problem::UnknownStrategy,
                    "unknown strategy",
                ))
            }
        };
        let allow_hooks = request
            .params
            .get("allowHooks")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let staging = Path::new(path).canonicalize().map_err(|error| {
            ProblemError::new(
                Problem::Internal,
                format!("staging path {}: {}", path, error),
            )
        })?;
        let plugins = paths::plugins_dir().canonicalize().map_err(|error| {
            ProblemError::new(Problem::Internal, format!("plugins dir: {}", error))
        })?;
        if !staging.starts_with(&plugins)
            || !staging
                .file_name()
                .map(|name| name.to_string_lossy().starts_with(".staging-"))
                .unwrap_or(false)
        {
            return Err(ProblemError::new(
                Problem::Internal,
                "staging path is outside the plugin staging area",
            ));
        }

        let staged = package::load_staged(&staging, settings::allow_insecure_install())?;
        let id = staged.manifest.name.clone();
        let version = staged.manifest.version.clone();
        if let Some(requested) = request.params.get("id").and_then(Value::as_str) {
            if requested != id {
                return Err(ProblemError::new(
                    Problem::Internal,
                    "staged id does not match the request",
                ));
            }
        }
        if let Some(requested) = request.params.get("version").and_then(Value::as_str) {
            if requested != version {
                return Err(ProblemError::new(
                    Problem::Internal,
                    "staged version does not match the request",
                ));
            }
        }
        if let Some(existing) = self.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::CoreService,
                    format!("id is a core service: {}", id),
                ));
            }
        }
        let host_interpreter =
            plugin::prepare_staged(&staged, allow_hooks)?;
        let signature_verified = staged.signature_verified;

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
        let service = plugin::install_staged(staged, strategy, allow_hooks, host_interpreter)?;
        if let Err(error) = service.carrier() {
            return Err(ProblemError::new(
                Problem::Internal,
                format!("plugin {}: {}", id, error),
            ));
        }
        if !still_running {
            self.slots.remove(&id);
            if let Some(exec) = service.exec.clone() {
                let mut slot =
                    Slot::new(id.clone(), exec, service.lifecycle.clone(), service.restart);
                slot.set_protocol(service.protocol);
                slot.set_protocol_only(true);
                slot.set_remain_after_exit(service.remain_after_exit);
                slot.set_wake(self.wake.clone());
                slot.set_stdio(self.stdio.clone());
                slot.set_raw_hub(Some(self.raw_hub.clone()));
                // A slot created by a runtime install needs the same contract,
                // projector, call budget and notification sink the boot path
                // gives it. Without this the plugin answers calls but its
                // notifications never reach the UI bus, and default_timeout is
                // not applied to its calls.
                configure_slot(&mut slot, &service);
                self.slots.insert(id.clone(), slot);
            }
        }
        self.registry
            .replace(service)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        self.bump_catalog();
        log::svcd(&format!(
            "svcd: plugin {} {} installed ({}){}",
            id,
            version,
            if still_running {
                "running; next start uses"
            } else {
                "now"
            },
            if signature_verified {
                ""
            } else {
                "; signature was NOT verified (allow_insecure_install)"
            }
        ));
        self.status_for_id(&id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    fn uninstall_plugin(&mut self, request: &rpc::Request) -> Result<(), ProblemError> {
        let id = request
            .params
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProblemError::new(Problem::Internal, "plugin.uninstall requires id")
            })?;
        let purge = request
            .params
            .get("purge")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let service = self.registry.get(id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })?;
        if service.kind != Kind::Plugin {
            return Err(ProblemError::new(
                Problem::NotAPlugin,
                format!("not a plugin: {}", id),
            ));
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
        runtime::remove(id);
        self.persisted.remove(id);
        self.bump_catalog();
        if purge {
            plugin::remove_logs(id);
            #[cfg(feature = "http")]
            crate::uichannel::forget(id);
        }
        log::svcd(&format!("svcd: plugin {} uninstalled", id));
        Ok(())
    }

    fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Err(ProblemError::new(
                Problem::Conflict,
                "the built-in svc service cannot be enabled or disabled",
            ));
        }
        let kind = self
            .registry
            .get(id)
            .map(|service| service.kind)
            .ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            })?;
        let (marker, create) = match kind {
            Kind::Plugin => (plugin::plugin_dir(id).join("enabled"), enabled),
            Kind::Core => (
                paths::services_dir().join(format!("{}.disabled", id)),
                !enabled,
            ),
        };
        if create {
            if let Some(parent) = marker.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    ProblemError::new(
                        Problem::Internal,
                        format!("cannot create {}: {}", parent.display(), error),
                    )
                })?;
            }
            fs::write(&marker, b"").map_err(|error| {
                ProblemError::new(
                    Problem::Internal,
                    format!("cannot write {}: {}", marker.display(), error),
                )
            })?;
        } else if marker.exists() {
            fs::remove_file(&marker).map_err(|error| {
                ProblemError::new(
                    Problem::Internal,
                    format!("cannot remove {}: {}", marker.display(), error),
                )
            })?;
        }
        if let Some(service) = self.registry.get_mut(id) {
            service.enabled = enabled;
        }
        // `disable` changes the boot flag only. The crash-restart policy is a
        // separate service property and must not be turned off behind the
        // operator's back.
        self.status_for_id(id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    /// `svc restart` = stop then start, on one RPC so no other caller can slip
    /// in between. A finished one-shot is stopped (settles immediately) and
    /// then runs again.
    fn restart_service(&mut self, id: &str) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id).expect("builtin"));
        }
        self.stop_service(id)?;
        self.start_service(id)
    }

    fn stop_service(&mut self, id: &str) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id).expect("builtin"));
        }
        let Some(state) = self.slots.get(id).map(|slot| slot.machine.state) else {
            return self.status_for_id(id).ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            });
        };

        let now = self.now_ms();
        self.slots.get_mut(id).expect("slot").request_stop(now);
        if matches!(state, State::Stopped | State::Failed) {
            return self.status_for_id(id).ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            });
        }
        let deadline = now
            .saturating_add(supervisor::STOP_TIMEOUT_MS)
            .saturating_add(supervisor::KILL_TIMEOUT_MS)
            .saturating_add(1_000);
        loop {
            self.pump();
            match self.slots[id].machine.state {
                State::Stopped => break,
                State::Failed => {
                    return Err(ProblemError::new(
                        Problem::Internal,
                        format!("service {} did not stop", id),
                    ))
                }
                _ => {}
            }
            if self.now_ms() >= deadline {
                return Err(ProblemError::new(
                    Problem::Timeout,
                    format!("stop timed out: {}", id),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.status_for_id(id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    fn pump(&mut self) {
        // Plugin stdio belongs to this loop: one non-blocking pass before every
        // pump keeps plugin pipes moving inside a synchronous start/stop wait.
        // The reader-thread target needs no equivalent — its threads deliver
        // into the slot channels on their own.
        self.stdio.pump();
        let now = self.now_ms();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        let mut changed = false;
        for id in ids {
            let mut slot = self.slots.remove(&id).expect("slot");
            let before = slot.machine.state;
            slot.pump(now);
            if slot.machine.state != before {
                changed = true;
            }
            self.slots.insert(id, slot);
        }
        if changed {
            self.bump_catalog();
        }
        self.persist_states();
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
    result: Result<Status, ProblemError>,
) -> rpc::Response {
    match result {
        Ok(status) => match serde_json::to_value(status) {
            Ok(value) => rpc::ok(id, value),
            Err(error) => rpc::err_data(
                id,
                -32603,
                &error.to_string(),
                problem_data(Problem::Internal),
            ),
        },
        Err(error) => problem_response(id, error_code, error),
    }
}

fn problem_data(problem: Problem) -> Value {
    json!({ "kind": problem.as_str() })
}

/// One management error -> one JSON-RPC frame. Funnelling every dispatch arm
/// through here keeps the `data.kind` and the code together.
fn problem_response(id: Option<Value>, code: i32, error: ProblemError) -> rpc::Response {
    rpc::err_data(id, code, error.detail(), problem_data(error.problem))
}

fn status_json(status: Status) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

/// Face-side projection of a completed slot call. The slot only emits a
/// `Result<Value, Failure>`; MCP codes/strings live here.
fn project_slot_response(
    external_id: Option<Value>,
    outcome: Result<Value, Failure>,
) -> rpc::Response {
    match outcome {
        Ok(value) => rpc::ok(external_id, value),
        Err(failure) => match failure.class {
            Class::Busy => rpc::err_data(
                external_id,
                -32000,
                "busy: too many in-flight tool calls",
                json!({ "class": failure.class.as_str() }),
            ),
            Class::FrameTooLarge => rpc::err_data(
                external_id,
                -32602,
                "tools/call request exceeds the 1 MiB frame limit",
                json!({ "class": failure.class.as_str() }),
            ),
            Class::NotRunning | Class::WriteFailed => {
                mcp::tool_result(external_id, failure.detail, true)
            }
            _ => mcp::tool_error(external_id, &failure.detail),
        },
    }
}

fn project_slot_outcome(external_id: Option<Value>, outcome: Result<Value, Failure>) -> String {
    rpc::to_line(&project_slot_response(external_id, outcome))
}

/// Build the completion sink for one tools/call: a stream reply is an
/// implementation error, and the final value/failure is projected by the face.
fn projected_done(
    reply: crate::channel::Reply,
    external_id: Option<Value>,
    stream_error: &'static str,
    project: fn(Option<Value>, Result<Value, Failure>) -> String,
) -> ReplySink {
    Box::new(move |outcome| {
        let outcome = match outcome {
            Ok(Reply::Value(value)) => Ok(value),
            Ok(Reply::Stream(_)) => Err(Failure::new(
                Class::ImplementationError,
                Retry::Never,
                stream_error,
            )),
            Err(failure) => Err(failure),
        };
        let _ = reply.send(project(external_id, outcome));
    })
}

/// Face-side projection of an immediate `Instance::call` rejection: the call was
/// never attempted, so it has no [`Failure`]. The wire shapes stay the ones the
/// old `rejection_failure` path produced (BadArguments through the frame-limit
/// error, UnknownCapability through the isError tool result); only the
/// structured `data.kind` is added to the JSON-RPC error.
fn project_rejection(external_id: Option<Value>, rejection: Rejection) -> rpc::Response {
    let kind = rejection.as_str();
    match rejection {
        Rejection::BadArguments { .. } => rpc::err_data(
            external_id,
            -32602,
            "tools/call request exceeds the 1 MiB frame limit",
            json!({ "kind": kind }),
        ),
        Rejection::UnknownCapability { capability } => {
            mcp::tool_error(external_id, &format!("unknown capability {capability}"))
        }
    }
}

/// Project a builtin call the same way the MCP face projects a slot call:
/// success becomes a text tool result, failures keep the classified envelope.
fn project_builtin_outcome(external_id: Option<Value>, outcome: Result<Value, Failure>) -> String {
    match outcome {
        Ok(value) => rpc::to_line(&mcp::tool_result(external_id, value.to_string(), false)),
        Err(failure) => project_slot_outcome(external_id, Err(failure)),
    }
}

/// Give a slot its contract and the face-side projections. The slot itself no
/// longer knows MCP or the UI bus.
fn configure_slot(slot: &mut Slot, service: &Service) {
    slot.set_contract(service.contract.clone());
    slot.set_call_timeout(crate::limits::current().default_timeout);
    slot.set_projector(Arc::new(project_slot_outcome));
    let id = service.id.clone();
    slot.set_notifier(Arc::new(move |method, params| {
        #[cfg(feature = "http")]
        crate::uichannel::publish(&id, method, params);
        #[cfg(not(feature = "http"))]
        {
            let _ = (method, params);
        }
    }));
}

fn send(reply: &crate::channel::Reply, response: rpc::Response) {
    reply.send(rpc::to_line(&response));
}

fn unknown_tool(id: &Option<Value>, name: &str) -> rpc::Response {
    rpc::err(id.clone(), -32602, &format!("unknown tool: {}", name))
}

/// The status action is a detail supplement, not a health gate: its stdout
/// becomes the `probe` text (capped), and a non-zero exit only appends the
/// status word. Capture goes through a temp file so a hook that leaves a child
/// holding stdout cannot block the daemon's event loop.
fn run_status_hook(id: &str, hook: &Hook, poll: &mut dyn FnMut()) -> String {
    let output_path = status_output_path(id);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = match options.open(&output_path) {
        Ok(file) => file,
        Err(error) => return format!("cannot create status output: {}", error),
    };
    let mut command = hook_command(id, "status", hook);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&output_path);
            return format!("spawn failed: {}", error);
        }
    };
    let _ = crate::platform::host::process::adopt(&child);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = read_status_output(&output_path);
                let _ = fs::remove_file(&output_path);
                return status_detail(&output, Some(status));
            }
            Ok(None) => {}
            Err(error) => {
                let output = read_status_output(&output_path);
                let _ = fs::remove_file(&output_path);
                let detail = status_detail(&output, None);
                return format!("{} (error: {})", detail, error);
            }
        }
        if started.elapsed() >= Duration::from_millis(hook.timeout_ms) {
            // The probe runs in its own process group: stop the whole tree, not
            // just the leader, or a status hook's children outlive the timeout.
            crate::hook::kill(&mut child);
            let _ = child.wait();
            let output = read_status_output(&output_path);
            let _ = fs::remove_file(&output_path);
            let detail = status_detail(&output, None);
            return if detail.is_empty() {
                "timed out".into()
            } else {
                format!("{} (timed out)", detail)
            };
        }
        // The supervisor loop owns plugin stdio; a probe must not freeze it.
        poll();
        thread::sleep(Duration::from_millis(50));
    }
}

fn status_output_path(id: &str) -> PathBuf {
    let directory = paths::config_dir().join("tmp");
    let _ = fs::create_dir_all(&directory);
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    directory.join(format!(
        "status-{}-{}-{}.out",
        id,
        std::process::id(),
        nanos
    ))
}

fn read_status_output(path: &Path) -> String {
    use std::io::Read;

    let Ok(file) = fs::File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let _ = file.take(8 * 1024).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).to_string()
}

fn status_detail(output: &str, status: Option<std::process::ExitStatus>) -> String {
    let text = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    match (text.is_empty(), status) {
        (true, Some(status)) if status.success() => "ok".into(),
        (true, Some(status)) => format!("failed ({})", status),
        (true, None) => String::new(),
        (false, Some(status)) if status.success() => text,
        (false, Some(status)) => format!("{} ({})", text, status),
        (false, None) => text,
    }
}


fn param_id(request: &rpc::Request) -> Result<String, rpc::Response> {
    request
        .params
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            rpc::err_data(
                request.id.clone(),
                -32602,
                "missing id",
                problem_data(Problem::Invalid),
            )
        })
}

fn start_layers(registry: &Registry, ids: &[String]) -> Result<Vec<Vec<String>>, ProblemError> {
    let (closure, missing) = walk_dependencies(registry, ids);
    if !missing.is_empty() {
        return Err(ProblemError::new(
            Problem::MissingDependency,
            format!("missing dependencies: {}", missing.join(", ")),
        ));
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
            return Err(ProblemError::new(Problem::Internal, "dependency cycle"));
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

fn auth_json(status: &LanStatus) -> Value {
    json!({
        "bind": status.bind.to_string(),
        "requestedBind": status.requested_bind.to_string(),
        "authRequired": status.require_auth,
        "passwordSet": status.password_set,
    })
}

fn missing_dependencies(registry: &Registry, id: &str) -> Vec<String> {
    walk_dependencies(registry, &[id.to_string()]).1
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::contract::{
        Capability as KernelCapability, CapabilityName, Effect,
    };
    use crate::registry::{Exec, Kind, Ready, Service};

    fn capability(name: &str) -> KernelCapability {
        KernelCapability {
            name: CapabilityName::new(name),
            desc: format!("{} description", name),
            input_schema: Some(json!({"type": "object"})),
            effect: Effect::Write,
            idempotent: false,
        }
    }

    fn plugin(id: &str, caps: &[&str]) -> Service {
        let caps = caps.iter().map(|cap| capability(cap)).collect();
        Service {
            kind: Kind::Plugin,
            contract: crate::registry::contract_for(
                id,
                "1.0.0",
                caps,
                crate::registry::Carrier::Slot,
            ),
            ..service(id, &[])
        }
    }

    fn service(id: &str, deps: &[&str]) -> Service {
        let contract = crate::registry::contract_for(
            id,
            "1.0.0",
            Vec::new(),
            crate::registry::Carrier::Slot,
        );
        Service {
            id: id.into(),
            kind: Kind::Core,
            version: "1.0.0".into(),
            protocol: 1,
            desc: "test".into(),
            deps: deps.iter().map(|dep| dep.to_string()).collect(),
            contract,
            impl_ref: None,
            exec: Some(Exec {
                path: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
                interpreter: None,
            }),
            lifecycle: None,
            enabled: true,
            restart: crate::registry::Restart::OnFailure,
            remain_after_exit: false,
            compat: None,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn the_daemon_does_not_spawn_call_worker_threads() {
        let (_sender, receiver) = mpsc::channel();
        let _daemon = Daemon::new(Registry::with_builtin(), receiver, None);
        let names: Vec<String> = std::fs::read_dir("/proc/self/task")
            .expect("task dir")
            .filter_map(|entry| {
                let path = entry.ok()?.path().join("comm");
                std::fs::read_to_string(path).ok()
            })
            .map(|name| name.trim().to_string())
            .collect();
        assert!(
            !names.iter().any(|name| name.starts_with("svcd-call")),
            "daemon must not spawn blocking call workers: {names:?}"
        );
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

        assert_eq!(error.problem, Problem::MissingDependency);
        assert!(error.detail().contains("missing dependencies:"), "{error}");
        assert!(error.detail().contains("first"), "{error}");
        assert!(error.detail().contains("second"), "{error}");
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
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n1 | cut -d: -f2)
  case "$line" in
    *svcd.ping*) printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id" ;;
    *'"name":"echo"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}],"isError":false}}\n' "$id" ;;
    *'"name":"fail"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"boom"}}\n' "$id" ;;
    *'"name":"hang"'*) ;;
    *'"name":"crash"'*) exit 0 ;;
    *'"name":"progress"'*)
      meta=$(printf '%s' "$line" | sed -n 's/.*"_meta":{\([^}]*\)}.*/\1/p')
      token=$(printf '%s' "$meta" | sed -n 's/.*"progressToken":"\([^"]*\)".*/\1/p')
      if [ -z "$token" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32602,"message":"progressToken must be under _meta"}}\n' "$id"
      else
        printf '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"%s"}}\n' "$token"
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"done"}],"isError":false}}\n' "$id"
      fi
      ;;
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
                interpreter: None,
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
    } elseif ($name -eq 'progress') {
      $token = $msg.params._meta.progressToken
      if ($null -ne $token) {
        [Console]::Out.WriteLine('{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"' + $token + '"}}')
        [Console]::Out.Flush()
      }
      $out = '{"jsonrpc":"2.0","id":' + $msg.id + ',"result":{"content":[{"type":"text","text":"done"}],"isError":false}}'
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
                interpreter: None,
            }
        }
    }

    fn tool_service(id: &str, kind: Kind, caps: &[&str], root: &Path) -> Service {
        let caps = caps.iter().map(|cap| capability(cap)).collect();
        let contract = crate::registry::contract_for(
            id,
            "1.0.0",
            caps,
            crate::registry::Carrier::Slot,
        );
        Service {
            id: id.into(),
            kind,
            version: "1.0.0".into(),
            protocol: 1,
            desc: "tools".into(),
            deps: Vec::new(),
            contract,
            impl_ref: None,
            exec: Some(tool_agent(root)),
            lifecycle: None,
            enabled: true,
            restart: crate::registry::Restart::OnFailure,
            remain_after_exit: false,
            compat: None,
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

    fn send(daemon: &mut Daemon, method: &str, params: Value) -> Receiver<String> {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .expect("request");
        let (sender, receiver) = mpsc::channel();
        daemon.handle(channel::PendingCall {
            endpoint: channel::Endpoint::Admin,
            line,
            reply: crate::channel::Reply::plain(sender),
        });
        receiver
    }

    /// Dispatch one request as if it arrived on `endpoint`.
    fn send_on(
        daemon: &mut Daemon,
        endpoint: channel::Endpoint,
        method: &str,
        params: Value,
    ) -> serde_json::Value {
        let line = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }))
        .expect("request");
        let (sender, receiver) = mpsc::channel();
        daemon.handle(channel::PendingCall {
            endpoint,
            line,
            reply: crate::channel::Reply::plain(sender),
        });
        let line = receiver.recv().expect("reply");
        serde_json::from_str(&line).expect("response")
    }

    /// Assert one management error's JSON-RPC code and structured kind.
    fn assert_error_kind(response: &Value, code: i32, kind: &str) {
        assert_eq!(response["error"]["code"], code, "{response}");
        assert_eq!(response["error"]["data"]["kind"], kind, "{response}");
    }

    /// §6: the read endpoint may drive lifecycle, never a write method.
    #[test]
    fn the_read_endpoint_cannot_run_a_write_method() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);

        let refused = send_on(
            &mut daemon,
            channel::Endpoint::User,
            "plugin.uninstall",
            json!({ "id": "nope" }),
        );
        assert_eq!(
            refused["error"]["message"], "unauthorized",
            "the read endpoint must not reach a write method: {refused}"
        );
        assert_eq!(
            refused["error"]["data"]["kind"], "internal",
            "the unauthorized gate is a control-plane failure: {refused}"
        );

        let allowed = send_on(
            &mut daemon,
            channel::Endpoint::User,
            "svc.list",
            json!({}),
        );
        assert!(
            allowed.get("error").is_none(),
            "the read endpoint must still list services: {allowed}"
        );
    }

    /// The same write method is not refused on the admin endpoint; it fails on
    /// the missing plugin instead of on the tier.
    #[test]
    fn the_admin_endpoint_reaches_a_write_method() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);

        let response = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.uninstall",
            json!({ "id": "nope" }),
        );
        assert_ne!(
            response["error"]["message"], "unauthorized",
            "the admin endpoint must get past the tier check: {response}"
        );
    }

    fn next_line(daemon: &mut Daemon, receiver: &Receiver<String>) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(line) = receiver.try_recv() {
                return serde_json::from_str(&line).expect("line json");
            }
            daemon.pump();
            assert!(Instant::now() < deadline, "no line");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait(daemon: &mut Daemon, receiver: &Receiver<String>) -> rpc::Response {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(line) = receiver.try_recv() {
                let value: Value = serde_json::from_str(&line).expect("response json");
                // Progress notifications share the caller's channel; skip
                // them until the final JSON-RPC response arrives. Any other
                // notification is unexpected and must fail the test.
                if let Some(method) = value.get("method").and_then(Value::as_str) {
                    if method == "notifications/progress" {
                        continue;
                    }
                    panic!("unexpected notification {method}: {value}");
                }
                return serde_json::from_value(value).expect("response");
            }
            daemon.pump();
            assert!(Instant::now() < deadline, "no response");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_tool_error(daemon: &mut Daemon, name: &str, expected: &str) {
        let call = send(daemon, "tools/call", json!({"name": name}));
        let result = wait(daemon, &call).result.expect("failure result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], expected);
    }

    #[test]
    fn a_live_plugin_routes_tool_calls_and_fails_them_on_a_crash() {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            crate::test_support::temp_dir().join(format!("svcd-tools-unit-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("logs")).expect("logs");
        std::env::set_var("SVCD_LOG_DIR", root.join("logs"));

        let plugin = tool_service(
            "plug",
            Kind::Plugin,
            &["echo", "fail", "hang", "crash"],
            &root,
        );
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![plugin]), receiver, None);
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

        assert_tool_error(&mut daemon, "plug.fail", "boom");

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
    fn a_core_service_with_capabilities_is_callable_through_the_slot_path() {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            crate::test_support::temp_dir().join(format!("svcd-tools-unit-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("logs")).expect("logs");
        std::env::set_var("SVCD_LOG_DIR", root.join("logs"));

        let core = tool_service("core", Kind::Core, &["echo"], &root);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![core]), receiver, None);
        daemon.start_service("core").expect("start");
        assert!(daemon.is_live("core"));

        let tools = daemon.tool_list();
        assert!(
            tools.iter().any(|tool| tool["name"] == "core.echo"),
            "{tools:?}"
        );
        let echo = send(
            &mut daemon,
            "tools/call",
            json!({"name": "core.echo", "arguments": {}}),
        );
        let response = wait(&mut daemon, &echo);
        assert_eq!(
            response.result.expect("echo result")["content"][0]["text"],
            "pong"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // Covers progress forwarding, failure projection, and child cleanup
    // through the same tools/call seam. Timeout/cancellation live in slot.rs:
    // tools/call has no per-call timeout or cancel entry.
    #[test]
    fn an_exec_core_service_forwards_progress_and_cleans_up_its_child() {
        let nanos = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            crate::test_support::temp_dir().join(format!("svcd-tools-unit-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("logs")).expect("logs");
        std::env::set_var("SVCD_LOG_DIR", root.join("logs"));

        let service = tool_service("proc", Kind::Core, &["progress", "fail"], &root);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![service]), receiver, None);
        daemon.start_service("proc").expect("start");
        assert!(daemon.is_live("proc"));
        let pid = daemon
            .status_for_id("proc")
            .expect("status")
            .summary
            .pid
            .expect("the slot owns a child pid");

        let call = send(
            &mut daemon,
            "tools/call",
            json!({
                "name": "proc.progress",
                "arguments": {},
                "progressToken": "t1"
            }),
        );
        let notification = next_line(&mut daemon, &call);
        assert_eq!(notification["method"], "notifications/progress");
        assert_eq!(notification["params"]["progressToken"], "t1");
        assert_eq!(notification["params"]["progress"], 1);

        let response = wait(&mut daemon, &call);
        assert_eq!(
            response.result.expect("progress result")["content"][0]["text"],
            "done"
        );

        // The same tools/call seam must project a capability failure as a
        // tool error, not only a successful value.
        assert_tool_error(&mut daemon, "proc.fail", "boom");

        daemon.stop_service("proc").expect("stop");
        let status = daemon.status_for_id("proc").expect("status");
        assert_eq!(status.summary.state, State::Stopped);
        assert!(status.summary.pid.is_none());
        #[cfg(unix)]
        assert!(
            !crate::platform::host::process::alive(pid),
            "the child process is still alive after stop"
        );
        // Windows has no portable liveness probe here; the per-service Job
        // Object terminates the tree, and status.pid is the observable seam.
        #[cfg(not(unix))]
        let _ = pid;

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tool_projection_follows_the_live_state() {
        let registry = registry(vec![plugin("plug", &["echo"])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, receiver, None);

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
    fn an_eighty_plugin_catalog_is_returned_in_bounded_pages() {
        let services: Vec<Service> = (0..80)
            .map(|index| {
                let mut service = plugin(&format!("p{index}"), &[]);
                let caps = (0..20)
                    .map(|cap| capability(&format!("cap{cap}")))
                    .collect();
                service.contract = crate::registry::contract_for(
                    &service.id,
                    "1.0.0",
                    caps,
                    crate::registry::Carrier::Slot,
                );
                service
            })
            .collect();
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(services), receiver, None);
        for slot in daemon.slots.values_mut() {
            slot.machine.state = State::Running;
        }

        let mut cursor = None;
        let mut total = 0usize;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"limit": 100, "cursor": cursor}),
                None => json!({"limit": 100}),
            };
            let page = daemon.tools_page(&params).expect("page");
            let tools = page["tools"].as_array().expect("tools");
            assert!(!tools.is_empty());
            assert!(
                serde_json::to_vec(tools).expect("serialize").len() <= crate::ipc::CATALOG_PAGE_MAX
            );
            total += tools.len();
            cursor = page["nextCursor"].as_str().map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(total, 80 * 21);
    }

    #[test]
    fn tool_pages_are_stable_and_reject_stale_cursors() {
        let registry = registry(vec![
            plugin("a", &["x"]),
            plugin("b", &["y"]),
            plugin("c", &["z"]),
        ]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, receiver, None);

        let first = daemon.tools_page(&json!({"limit": 2})).expect("first page");
        assert_eq!(first["tools"].as_array().expect("tools").len(), 2);
        let cursor = first["nextCursor"]
            .as_str()
            .expect("next cursor")
            .to_string();

        let second = daemon
            .tools_page(&json!({"limit": 2, "cursor": cursor}))
            .expect("second page");
        assert_eq!(second["tools"].as_array().expect("tools").len(), 1);
        assert!(second.get("nextCursor").is_none());

        let stale = daemon
            .tools_page(&json!({"limit": 2, "cursor": "bm90LWEtY3Vyc29y"}))
            .expect_err("invalid cursor");
        assert_eq!(stale.problem, Problem::Invalid);
        assert!(stale.detail().contains("cursor"), "{stale}");

        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let wrong_version = URL_SAFE_NO_PAD.encode("999:0");
        let stale = daemon
            .tools_page(&json!({"limit": 2, "cursor": wrong_version}))
            .expect_err("version mismatch");
        assert_eq!(stale.problem, Problem::Internal);
        let past_end = URL_SAFE_NO_PAD.encode(format!("{}:99", daemon.catalog_version));
        let stale = daemon
            .tools_page(&json!({"limit": 2, "cursor": past_end}))
            .expect_err("past end");
        assert_eq!(stale.problem, Problem::Internal);
    }

    #[test]
    fn tools_list_and_unknown_tool_use_the_documented_codes() {
        let registry = registry(vec![plugin("plug", &["echo"])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, receiver, None);

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
        let mut daemon = Daemon::new(registry, receiver, None);

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
        let mut daemon = Daemon::new(registry, receiver, None);

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
        let mut daemon = Daemon::new(registry, receiver, None);

        let response = send(&mut daemon, "tools/call", json!({"name": "core.start"}));
        let response = wait(&mut daemon, &response);
        assert_eq!(response.error.expect("error").code, -32602);
    }

    #[test]
    fn start_refuses_a_failed_dependency() {
        let registry = registry(vec![service("app", &["base"]), service("base", &[])]);
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry, receiver, None);
        daemon.slots.get_mut("base").expect("base").machine.state = State::Failed;

        let error = daemon.start_service("app").expect_err("failed dependency");

        assert_eq!(error.problem, Problem::Internal);
        assert!(
            error.detail().contains("dependency base is failed"),
            "{}",
            error
        );
    }

    fn two_owners(id: &str) -> Service {
        Service {
            impl_ref: Some(crate::kernel::contract::ImplRef {
                tag: "builtin".into(),
                binding: json!({}),
            }),
            ..service(id, &[])
        }
    }

    #[test]
    fn a_reload_rejects_two_owners_and_leaves_the_live_service_and_slot_alone() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(
            registry(vec![service("live", &[])]),
            receiver,
            None,
        );

        let mut desired = BTreeMap::new();
        desired.insert("bad".to_string(), two_owners("bad"));
        let mut conflicting_live = two_owners("live");
        conflicting_live.version = "9.9.9".into();
        conflicting_live.contract = crate::registry::contract_for(
            "live",
            "9.9.9",
            conflicting_live.contract.surface().to_vec(),
            crate::registry::Carrier::Launcher,
        );
        desired.insert("live".to_string(), conflicting_live);

        let (added, updated) = daemon.apply_definitions(desired);

        assert!(
            added.is_empty(),
            "a rejected new service must not be reported as added"
        );
        assert!(
            updated.is_empty(),
            "a rejected update must not be reported as updated"
        );
        assert!(daemon.registry.get("bad").is_none());
        assert!(
            !daemon.slots.contains_key("bad"),
            "a rejected new service must not leave a slot behind"
        );
        let live = daemon.registry.get("live").expect("live service");
        assert_eq!(
            live.version, "1.0.0",
            "a rejected update must not reach the registry"
        );
        assert_eq!(live.carrier(), Ok(crate::registry::Carrier::Slot));
        assert!(
            daemon.slots.contains_key("live"),
            "the live slot must survive a rejected update"
        );
    }

    #[test]
    fn the_assembly_point_keeps_one_carrier_per_service() {
        let mut registry = Registry::with_builtin();
        registry.insert(service("core", &[])).expect("core");
        let (_sender, receiver) = mpsc::channel();
        let daemon = Daemon::new(registry, receiver, None);

        assert!(
            daemon.slots.contains_key("core"),
            "an exec service is owned by the supervised slot"
        );
        assert!(
            !daemon.slots.contains_key("svc"),
            "the builtin is launcher-backed and must never get a slot"
        );
        assert_eq!(
            daemon.registry.get("svc").map(Service::carrier),
            Some(Ok(crate::registry::Carrier::Launcher))
        );
        assert_eq!(
            daemon.registry.get("core").map(Service::carrier),
            Some(Ok(crate::registry::Carrier::Slot))
        );
    }

    #[test]
    fn the_builtin_svc_is_projected_and_called_through_launchers() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);

        let listed = send(&mut daemon, "tools/list", json!({}));
        let listed = wait(&mut daemon, &listed);
        let names: Vec<&str> = listed.result.as_ref().expect("list result")["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"svc.version"), "{names:?}");
        assert!(names.contains(&"svc.health"), "{names:?}");
        assert!(!names.contains(&"svc.start"), "{names:?}");

        let version = send(
            &mut daemon,
            "tools/call",
            json!({"name": "svc.version", "arguments": {}}),
        );
        let version = wait(&mut daemon, &version);
        let result = version.result.expect("version result");
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().expect("version text");
        let value: Value = serde_json::from_str(text).expect("version json");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));

        let health = send(
            &mut daemon,
            "tools/call",
            json!({"name": "svc.health", "arguments": {}}),
        );
        let health = wait(&mut daemon, &health);
        let result = health.result.expect("health result");
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().expect("health text");
        let value: Value = serde_json::from_str(text).expect("health json");
        assert_eq!(value["state"], "ready");
    }

    #[test]
    fn a_closed_channel_after_shutdown_is_not_reported_as_unreachable() {
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
        ] {
            assert!(shutdown_reply_lost(&std::io::Error::from(kind)), "{kind:?}");
        }
        // A pipe that was never available is a real "not running" failure.
        assert!(!shutdown_reply_lost(&std::io::Error::from(
            std::io::ErrorKind::TimedOut
        )));
        assert!(!shutdown_reply_lost(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[test]
    fn the_builtin_outcome_projects_values_and_failures() {
        let ok = project_builtin_outcome(Some(json!(1)), Ok(json!({"version": "1.2.3"})));
        let response: rpc::Response = serde_json::from_str(&ok).expect("ok json");
        let result = response.result.expect("result");
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "{\"version\":\"1.2.3\"}");

        let failed = project_builtin_outcome(
            Some(json!(2)),
            Err(Failure::new(Class::Busy, Retry::Never, "busy")),
        );
        let response: rpc::Response = serde_json::from_str(&failed).expect("error json");
        let error = response.error.expect("error");
        assert_eq!(error.code, -32000);
        assert_eq!(error.data.expect("data")["class"], "busy");
    }

    #[test]
    fn lifecycle_management_errors_carry_their_problem_kind() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);

        let started = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.start",
            json!({"id": "nope"}),
        );
        assert_error_kind(&started, -32003, "unknown_service");

        let stopped = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.stop",
            json!({"id": "nope"}),
        );
        assert_error_kind(&stopped, -32004, "unknown_service");

        let restarted = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.restart",
            json!({"id": "nope"}),
        );
        assert_error_kind(&restarted, -32010, "unknown_service");

        let status = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.status",
            json!({"id": "nope"}),
        );
        assert_error_kind(&status, -32001, "unknown_service");

        let enabled = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.enable",
            json!({"id": "svc"}),
        );
        assert_error_kind(&enabled, -32005, "conflict");

        let disabled = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.disable",
            json!({"id": "svc"}),
        );
        assert_error_kind(&disabled, -32005, "conflict");
    }

    #[test]
    fn plugin_management_errors_carry_their_problem_kind() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);

        let missing_path = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.install",
            json!({}),
        );
        assert_error_kind(&missing_path, -32006, "internal");

        let missing_staged_path = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.install_staged",
            json!({}),
        );
        assert_error_kind(&missing_staged_path, -32006, "internal");

        let unknown_strategy = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.install",
            json!({"path": "ignored", "strategy": "bogus"}),
        );
        assert_error_kind(&unknown_strategy, -32006, "unknown_strategy");

        let unknown_plugin = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.uninstall",
            json!({"id": "nope"}),
        );
        assert_error_kind(&unknown_plugin, -32007, "unknown_plugin");

        let missing_id = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.uninstall",
            json!({}),
        );
        assert_error_kind(&missing_id, -32007, "internal");
    }

    #[test]
    fn auth_and_parameter_errors_carry_their_problem_kind() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);

        let auth = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "auth.status",
            json!({}),
        );
        assert_error_kind(&auth, -32008, "internal");

        let password = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "auth.passwd",
            json!({"password": ""}),
        );
        assert_error_kind(&password, -32008, "internal");

        let missing_id = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.start",
            json!({}),
        );
        assert_error_kind(&missing_id, -32602, "invalid");

        let cursor = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "tools/list",
            json!({"cursor": "bm90LWEtY3Vyc29y"}),
        );
        assert_error_kind(&cursor, -32602, "invalid");

        let unknown_method = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "no.such.method",
            json!({}),
        );
        assert_error_kind(&unknown_method, -32601, "invalid");
    }

    #[test]
    fn uninstalling_a_core_service_is_not_a_plugin() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![service("core", &[])]), receiver, None);

        let response = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.uninstall",
            json!({"id": "core"}),
        );
        assert_error_kind(&response, -32007, "not_a_plugin");
    }

    #[test]
    fn a_start_with_a_missing_dependency_reports_missing_dependency() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![service("app", &["missing"])]), receiver, None);

        let response = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "svc.start",
            json!({"id": "app"}),
        );
        assert_error_kind(&response, -32003, "missing_dependency");
    }

    #[test]
    fn installing_a_package_named_after_a_core_service_reports_core_service() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);
        let dir = package::tempdir::TempDir::new();
        let path = dir.path().join("svc.spk");
        std::fs::write(
            &path,
            package::test_signed_package("svc", crate::platform::target()),
        )
        .expect("write package");

        let response = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.install",
            json!({"path": path.to_string_lossy(), "strategy": "replace"}),
        );
        assert_error_kind(&response, -32006, "core_service");
    }

    #[test]
    fn installing_a_package_for_another_platform_reports_unsupported_platform() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(Registry::with_builtin(), receiver, None);
        let dir = package::tempdir::TempDir::new();
        let path = dir.path().join("other.spk");
        std::fs::write(&path, package::test_signed_package("other", "other-triple"))
            .expect("write package");

        let response = send_on(
            &mut daemon,
            channel::Endpoint::Admin,
            "plugin.install",
            json!({"path": path.to_string_lossy(), "strategy": "replace"}),
        );
        assert_error_kind(&response, -32006, "unsupported_platform");
    }

    #[test]
    fn a_start_layer_past_its_deadline_projects_timeout_kind() {
        let (_sender, receiver) = mpsc::channel();
        let mut daemon = Daemon::new(registry(vec![service("stuck", &[])]), receiver, None);
        // Force a start that is in flight but never becomes ready; the deadline
        // is already past, so the layer must report a timeout without sleeping.
        daemon.slots.get_mut("stuck").expect("slot").machine.state = State::Starting;

        let error = daemon
            .start_layer(&["stuck".into()], 0)
            .expect_err("past deadline");
        assert_eq!(error.problem, Problem::Timeout);

        let response = status_response(Some(json!(7)), -32003, Err(error));
        let error = response.error.expect("error");
        assert_eq!(error.code, -32003);
        assert_eq!(error.data.expect("data")["kind"], "timeout");
    }

    #[test]
    fn a_typed_management_error_projects_its_wire_kind() {
        // The category is raised by the layer that rejected the input (#151);
        // the wire projection only reads it.
        let response = status_response(
            Some(json!(8)),
            -32006,
            Err(Problem::TooLarge.error("package exceeds 8 MiB")),
        );
        assert_eq!(
            response.error.expect("error").data.expect("data")["kind"],
            "too_large"
        );
    }

    #[test]
    fn the_slot_response_keeps_all_eight_class_shapes() {
        for class in Class::ALL {
            let detail = format!("detail {}", class.as_str());
            let response = project_slot_response(
                Some(json!(1)),
                Err(Failure::new(class, class.default_retry(), detail.clone())),
            );
            match class {
                Class::Busy => {
                    let error = response.error.expect("busy error");
                    assert_eq!(error.code, -32000);
                    assert_eq!(error.message, "busy: too many in-flight tool calls");
                    assert_eq!(error.data.expect("data")["class"], "busy");
                }
                Class::FrameTooLarge => {
                    let error = response.error.expect("frame error");
                    assert_eq!(error.code, -32602);
                    assert_eq!(
                        error.message,
                        "tools/call request exceeds the 1 MiB frame limit"
                    );
                    assert_eq!(error.data.expect("data")["class"], "frame_too_large");
                }
                _ => {
                    let result = response.result.expect("isError result");
                    assert_eq!(result["isError"], true);
                    assert_eq!(result["content"][0]["text"], detail);
                }
            }
        }
    }

    #[test]
    fn an_immediate_rejection_projects_its_kind_without_changing_the_message() {
        let response = project_rejection(
            Some(json!(5)),
            Rejection::BadArguments {
                detail: "arguments are too large".into(),
            },
        );
        let error = response.error.expect("bad arguments error");
        assert_eq!(error.code, -32602);
        assert_eq!(
            error.message,
            "tools/call request exceeds the 1 MiB frame limit"
        );
        assert_eq!(error.data.expect("data")["kind"], "bad_arguments");

        let response = project_rejection(
            Some(json!(6)),
            Rejection::UnknownCapability {
                capability: "nope".into(),
            },
        );
        let result = response.result.expect("unknown capability result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "unknown capability nope");
    }
}
