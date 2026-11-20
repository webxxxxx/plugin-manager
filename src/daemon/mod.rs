use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

#[cfg(feature = "http")]
use std::net::{SocketAddr, TcpListener};
#[cfg(feature = "http")]
use std::sync::Mutex;

#[cfg(not(feature = "http"))]
type Api = ();
use crate::face::catalog;
use crate::face::control;
#[cfg(feature = "http")]
use crate::face::fallback::respond;
use crate::face::mcp_tools;
#[cfg(feature = "http")]
use crate::face::prebody::PreBodyFaces;
use crate::face::uichannel::EventBus;
use crate::kernel::contract::{CapabilityName, Instance, Invocation, Launchers, Reply, ReplySink};
use crate::kernel::failure::{Class, Failure, Problem, ProblemError, Retry};
use crate::kernel::projection::{anchored_mono_ms, anchored_secs, anchored_secs_opt, Status};
use crate::kernel::registry::{Hook, Kind, Registry, Service, State, RESERVED_ID};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::Strategy;
use crate::platform::clock::{Clock, Now};
use crate::platform::compat;
use crate::platform::config::Config;
use crate::platform::host::wake::Wake;
use crate::platform::log;
use crate::platform::paths::Paths;
#[cfg(feature = "http")]
use crate::platform::settings;
#[cfg(feature = "http")]
use crate::server::auth::lan::Lan;
use crate::server::auth::lan::LanStatus;
use crate::supervise::check::{reconcile, start};
use crate::supervise::model::definition;
use crate::supervise::model::runtime::{self, Record};
use crate::supervise::model::supervisor::{self, Event};
use crate::supervise::ops::slot::hook_command;
use crate::supervise::ops::slot::{Slot, SlotWire};
#[cfg(unix)]
use crate::wire::contract::reactor::Reactor;
use crate::wire::control::channel;
use crate::wire::message::rpc;

/// The supervisor loop's bounded wait when it has no earlier deadline. With a
/// reactor this is the plugin poll's timeout, so an idle daemon wakes once per
/// tick: the poll doubles as the wait for shutdown and the loop's timers.
const POLL_WAIT: Duration = Duration::from_millis(50);

/// Anything that holds the single supervisor loop longer than this is logged.
///
/// The loop is serial, so one slow phase delays every other caller; the 30 s
/// management deadline then shows up as a 504 far from the real cause.
const SLOW_LOOP_MS: u64 = 1_000;

pub fn run(conf: Option<PathBuf>) -> i32 {
    // The directory set has to exist before `ensure_conf` can report the path
    // it settled on. Read the explicit document once when it exists; the
    // bootstrap below reuses that value instead of parsing the same file twice.
    // A missing `--conf` falls through to the normal discovery, exactly as
    // `ensure_conf` does.
    let explicit = conf.as_deref().filter(|path| path.is_file());
    let preloaded = match Config::bootstrap(explicit) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("svcd: invalid config: {}", error);
            return 1;
        }
    };
    let initial_paths = preloaded.paths.clone();
    if let Err(error) = fs::create_dir_all(&initial_paths.config_dir) {
        eprintln!(
            "svcd: cannot create config directory {}: {}",
            initial_paths.config_dir.display(),
            error
        );
        return 1;
    }

    let mut settings = (*preloaded.settings).clone();
    let config_path =
        match crate::selfinstall::ensure_conf(conf.as_deref(), &mut settings, &initial_paths) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("svcd: {}", error);
                return 1;
            }
        };
    // The generated or filled document is this start's document. Rebuilding
    // from the value `ensure_conf` just materialized keeps the first start on
    // the same roots as every later start (ADR-0023).
    let config = Arc::new(preloaded.rebind(settings, Some(config_path.clone())));
    if let Err(error) = log::init_svcd(&config.limits, &config.paths.logs_dir) {
        eprintln!("svcd: cannot open log directory: {}", error);
    }
    log::svcd("svcd starting");
    for warning in &config.warnings {
        log::svcd(&format!("svcd: warning: {}", warning));
    }
    // Runtime state no longer lives in the config tree: Unix sockets and the
    // HTTP port file need data/run to exist before the channel binds.
    for dir in [&config.paths.run_dir, &config.paths.temp_dir] {
        if let Err(error) = fs::create_dir_all(dir) {
            log::svcd(&format!("svcd: cannot create {}: {}", dir.display(), error));
        }
    }
    log::svcd(&format!(
        "svcd: config: {}",
        config
            .settings_path
            .as_deref()
            .unwrap_or(config_path.as_path())
            .display()
    ));
    // The derivation report comes after the log is open: the budget, the shape
    // and every rewritten value are the first thing a support engineer needs.
    // `lines()` already renders the override list, so it is not printed twice.
    for line in config.resources.lines() {
        log::svcd(&format!("svcd: {}", line));
    }
    let stated = config.settings.probe_overrides().stated_names();
    log::svcd(&format!(
        "svcd: probe inputs stated by svcd.conf: {}",
        if stated.is_empty() {
            "none".to_string()
        } else {
            stated.join(", ")
        }
    ));
    match crate::platform::limits::write_snapshot(&config.paths, &config.resources) {
        Ok(()) => {}
        Err(error) => log::svcd(&format!(
            "svcd: cannot write the resource snapshot: {}",
            error
        )),
    }
    // An explicit --conf may set config_dir to a different directory.
    let config_dir = &config.paths.config_dir;
    if config_dir != &initial_paths.config_dir {
        if let Err(error) = fs::create_dir_all(config_dir) {
            eprintln!(
                "svcd: cannot create config directory {}: {}",
                config_dir.display(),
                error
            );
            return 1;
        }
    }
    crate::manage::cleanup_staging(&config.paths);
    // The channel no longer uses a token: authorization is the endpoint ACL.
    // Remove whatever the previous version left behind so nothing keeps reading
    // a stale secret.
    let legacy_token = config_dir.join("svcd.token");
    if legacy_token.exists() {
        let _ = fs::remove_file(&legacy_token);
        log::svcd("svcd: removed the legacy control token (endpoint ACLs authorize now)");
    }
    #[cfg(windows)]
    if crate::platform::host::acl::is_machine_wide(config_dir) {
        if let Err(error) = crate::platform::host::acl::harden_directories(&config.paths) {
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

    wait_for_services_dir(
        &config.paths.services_dir,
        config.settings.boot_scan_wait_ms(),
    );

    let mut loaded = definition::load_services(&config.paths.services_dir, &config.limits);
    for issue in &loaded.issues {
        log::svcd(&format!("svcd: warning: {}", issue));
    }

    let (plugins, warnings) = plugin::scan(&config.paths, &config.limits);
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

    // The supervisor's pending management queue depth is derived with the rest
    // of the backpressure limits, so a full queue is the same visible busy
    // signal on every budget.
    let (tx, rx) = mpsc::sync_channel(config.limits.pending_queue);
    let wake = Arc::new(Wake::new());
    // One path on every host: the channel binds the endpoint and either hands
    // the listener to the server loop (Unix) or starts its own accept thread
    // (Windows). The composition root only sees the resulting server.
    let control = match channel::bind_server(
        config.paths.control_dir(),
        tx,
        wake.clone(),
        crate::platform::host::threads::io_stack(&config.limits),
    ) {
        Ok(control) => control,
        Err(error) => {
            log::svcd(&format!("svcd: cannot bind control channel: {}", error));
            return 1;
        }
    };
    #[cfg(feature = "http")]
    let raw_hub = crate::wire::contract::rawhub::RawHub::new(config.limits.clone());
    // The driver picks itself inside `server::http::entry`; the composition
    // root only hands over the control plumbing it owns.
    let bus = Arc::new(EventBus::new());
    #[cfg(feature = "http")]
    let stream_budget = crate::server::http::budget::StreamBudget::new();
    #[cfg(feature = "http")]
    let api = match spawn_http(
        config.clone(),
        raw_hub.clone(),
        control,
        bus.clone(),
        stream_budget,
    ) {
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
    let control_loop =
        match crate::server::control::spawn_control_only(control, config.limits.clone()) {
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
    let raw_hub = crate::wire::contract::rawhub::RawHub::new(config.limits.clone());
    #[cfg(not(feature = "http"))]
    let api: Option<Arc<Api>> = None;
    crate::platform::host::notify::systemd("READY=1");

    let mut daemon = Daemon::new_with_raw(
        loaded.registry,
        rx,
        api.clone(),
        raw_hub.clone(),
        config.clone(),
        bus,
    );
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
    let clock = Clock::boot();
    daemon.adopt_runtime(clock.now());
    daemon.sweep_runtime_records();
    crate::platform::host::signal::install();
    daemon.start_enabled(&clock);
    daemon.event_loop(&clock);
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
    crate::platform::host::notify::systemd("STOPPING=1");
    channel::remove_socket(config.paths.control_dir());
    0
}

/// The daemon's own handle on the HTTP surface: LAN state plus the driver that
/// stops the server loop. Assembly lives here (§5: `daemon.rs` is the
/// composition root); the driver itself lives in `server/http` (#205).
#[cfg(feature = "http")]
pub(crate) struct Api {
    lan: Arc<Mutex<Lan>>,
    /// Stops this host's connection driver on shutdown.
    driver: crate::server::http::entry::ServerHandle,
}

#[cfg(feature = "http")]
impl Api {
    pub(crate) fn set_password(&self, hash: String) -> Result<LanStatus, String> {
        let mut state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        state.set_password(hash);
        Ok(state.status())
    }

    pub(crate) fn status(&self) -> Result<LanStatus, String> {
        let state = self.lan.lock().map_err(|_| "LAN state poisoned")?;
        Ok(state.status())
    }

    /// Stop the server loop after the supervisor loop returned. On Unix this
    /// drains queued control replies and flushes them before the process exits.
    pub(crate) fn shutdown(&self) {
        self.driver.shutdown();
    }
}

#[cfg(feature = "http")]
fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}

#[cfg(feature = "http")]
fn resolve_bind(env: Option<&str>, configured: Option<SocketAddr>) -> SocketAddr {
    env.and_then(|value| value.parse::<SocketAddr>().ok())
        .or(configured)
        .unwrap_or_else(default_bind)
}

/// Compose the HTTP surface and start this host's connection driver.
///
/// `control` is the bound control channel; a poll-loop host consumes its
/// listener, a thread-per-connection host ignores it (the channel already
/// started its accept thread). Which driver runs is decided in
/// `server::http::entry`, not here.
#[cfg(feature = "http")]
fn spawn_http(
    config: Arc<Config>,
    raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    control: crate::wire::control::channel::ControlBinding,
    bus: Arc<EventBus>,
    budget: crate::server::http::budget::StreamBudget,
) -> std::io::Result<Arc<Api>> {
    let configured = config.settings.bind;
    let requested = resolve_bind(std::env::var("SVCD_BIND").ok().as_deref(), configured);
    let password_hash = config.settings.auth_passwd.clone();

    // A LAN bind without a password would expose an unauthenticated API. Keep
    // the daemon reachable on loopback so the operator can still run
    // `svc lan passwd`, then restart to pick up the requested bind.
    let mut bind = requested;
    if !requested.ip().is_loopback() && password_hash.is_none() {
        log::svcd(&format!(
            "svcd: bind {} requires auth_passwd; falling back to 127.0.0.1:{}",
            requested,
            requested.port()
        ));
        bind = SocketAddr::from(([127, 0, 0, 1], requested.port()));
    }
    let require_auth = !bind.ip().is_loopback();
    let mut state = Lan::new(bind, requested, require_auth, password_hash);
    // A failed bind disables HTTP only: the Unix loop still serves the CLI,
    // and the thread driver reports the same failure through `entry::start`.
    let listener = match TcpListener::bind(bind) {
        Ok(listener) => match listener.local_addr() {
            Ok(local) => {
                state.bind = local;
                let port_file = config.paths.run_dir.join("http.port");
                if let Err(error) = fs::write(&port_file, format!("127.0.0.1:{}", local.port())) {
                    log::svcd(&format!(
                        "svcd: cannot write {}: {}",
                        port_file.display(),
                        error
                    ));
                }
                log::svcd(&format!(
                    "svcd: HTTP API on {} (auth required: {})",
                    local, require_auth
                ));
                Some(listener)
            }
            Err(error) => {
                log::svcd(&format!("svcd: HTTP API disabled: {}", error));
                None
            }
        },
        Err(error) => {
            log::svcd(&format!("svcd: HTTP API disabled: {}", error));
            None
        }
    };

    let lan = Arc::new(Mutex::new(state));
    let fallback_config = config.clone();
    let face: Arc<dyn crate::server::http::Face> =
        Arc::new(crate::face::api::ApiFace::new(config.clone(), lan.clone()));
    let routes: Arc<dyn crate::server::http::hooks::PreBodyRoutes> = Arc::new(PreBodyFaces {
        config: config.clone(),
        bus: bus.clone(),
    });
    let fallback: crate::server::http::hooks::FallbackHandler =
        Arc::new(move |request, lan| respond(&fallback_config, request, lan));

    let driver = crate::server::http::entry::start(crate::server::http::entry::StartInput {
        config,
        lan: lan.clone(),
        raw_hub,
        listener,
        face,
        routes,
        fallback,
        control,
        budget,
    })?;
    Ok(Arc::new(Api { lan, driver }))
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

/// The service directory lives on the overlay on read-only-rootfs hosts, so it
/// can appear a few seconds after inittab starts svcd. Wait for it instead of
/// scanning an empty tree once and never looking again; `svc reload` covers the
/// rest. The budget comes from `svcd.conf`'s `boot_scan_wait_ms` (`0` disables
/// the wait; the `SVCD_BOOT_SCAN_WAIT_MS` override still wins at bootstrap).
fn wait_for_services_dir(dir: &Path, budget: u64) {
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

/// Ask the running daemon to shut down. The event loop checks this flag on
/// every wake, so a caller on any thread (the Windows SCM control handler,
/// for example) can stop it without touching daemon state.
pub use crate::platform::host::signal::request_shutdown;

pub fn shutdown() -> i32 {
    shutdown_inner(true)
}

/// Stop without printing "unreachable" when the daemon already exited.
pub fn shutdown_quiet() -> i32 {
    shutdown_inner(false)
}

fn shutdown_inner(verbose: bool) -> i32 {
    // Stopping only needs the control endpoint; a broken `svcd.conf` must not
    // make `svcd stop` unusable.
    let paths = crate::platform::config::ambient_paths(None);
    match channel::request(paths.control_dir(), "svcd.shutdown", json!({})) {
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

struct Daemon {
    /// The one derived runtime configuration: settings file, directory set and
    /// limits. Every directory this process touches comes from here.
    config: Arc<Config>,
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
    stdio: crate::wire::contract::stdio::Target,
    raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
    /// The one UI event bus built by the composition root.
    bus: Arc<EventBus>,
}

impl Daemon {
    #[cfg(test)]
    fn new(registry: Registry, rx: Receiver<channel::PendingCall>, api: Option<Arc<Api>>) -> Self {
        let config = Arc::new(Config::with_paths(
            crate::platform::settings::Settings::default(),
            None,
            {
                let nanos = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let root = crate::test_support::temp_dir().join(format!(
                    "svcd-daemon-unit-{}-{}",
                    std::process::id(),
                    nanos
                ));
                let paths = Paths::under(&root);
                crate::manage::test_support::install_test_trust(&paths.trust_dir);
                paths
            },
        ));
        Self::new_with_raw(
            registry,
            rx,
            api,
            crate::wire::contract::rawhub::RawHub::new(config.limits.clone()),
            config,
            Arc::new(EventBus::new()),
        )
    }

    fn new_with_raw(
        registry: Registry,
        rx: Receiver<channel::PendingCall>,
        api: Option<Arc<Api>>,
        raw_hub: Arc<crate::wire::contract::rawhub::RawHub>,
        config: Arc<Config>,
        bus: Arc<EventBus>,
    ) -> Self {
        let wake = Arc::new(Wake::new());
        let rescan_interval_ms = config.settings.rescan_interval_ms();
        let mut slots = BTreeMap::new();
        for service in registry.managed_values() {
            if let Some(exec) = service.exec.clone() {
                let mut slot = Slot::new(
                    service.id.clone(),
                    exec,
                    service.lifecycle.clone(),
                    service.restart,
                    config.paths.clone(),
                    config.limits.clone(),
                );
                slot.set_protocol(service.protocol);
                slot.set_protocol_only(service.kind == Kind::Plugin);
                slot.set_remain_after_exit(service.remain_after_exit);
                slot.set_initial_state(service.state);
                slot.set_wake(wake.clone());
                configure_slot(&mut slot, service, &config.limits, &bus);
                slots.insert(service.id.clone(), slot);
            }
        }
        let temp_retention = config.settings.temp_retention();
        let temp_cleanup_interval_ms = config.settings.temp_cleanup_interval().as_millis() as u64;
        let services_fingerprint = services_fingerprint(&config.paths.services_dir);
        let mut launchers = Launchers::default();
        launchers.register(
            "builtin",
            Box::new(crate::supervise::ops::builtin_adapter::BuiltinLauncher),
        );
        Daemon {
            config,
            registry,
            rx,
            api,
            wake,
            slots,
            launchers: Arc::new(launchers),
            shutdown: false,
            catalog_version: 1,
            catalog_cache: None,
            persisted: BTreeMap::new(),
            boot_id: crate::platform::host::process::boot_id(),
            services_fingerprint,
            rescan_interval_ms,
            next_rescan_ms: rescan_interval_ms,
            temp_retention,
            temp_cleanup_interval_ms,
            // The old cleanup thread ran once before its first sleep; the
            // supervisor does the same by treating the first tick as due.
            next_temp_cleanup_ms: 0,
            #[cfg(unix)]
            reactor: None,
            stdio: crate::wire::contract::stdio::Target::threaded(),
            raw_hub,
            bus,
        }
    }

    /// Probe a recorded pid once: the model takes a `runtime::ProcessStamp` and
    /// never touches the OS.
    fn process_probe(pid: u32) -> runtime::ProcessStamp {
        if crate::platform::host::process::alive(pid) {
            runtime::ProcessStamp::Alive {
                starttime: crate::platform::host::process::start_time(pid),
            }
        } else {
            runtime::ProcessStamp::Gone
        }
    }

    /// The adoption-side guard: a record with no `since` stays "never
    /// happened" (`0`) instead of being anchored to this daemon's boot. One
    /// helper so both adoption branches agree.
    fn adopted_since(now: Now, record: &Record) -> u64 {
        anchored_mono_ms(now, record.since).unwrap_or(0)
    }

    /// Reconnect to processes that survived a previous daemon. A record is only
    /// trusted when its boot id matches, the pid is alive and the pid's start
    /// time still matches (a reused pid fails this check). On Windows the job
    /// objects kill the tree with the daemon, so there is nothing to adopt.
    fn adopt_runtime(&mut self, now: Now) {
        let boot = self
            .boot_id
            .clone()
            .or_else(crate::platform::host::process::boot_id);
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            let Some(record) = runtime::load(&id, &self.config.paths) else {
                continue;
            };
            if !runtime::same_boot(&record, boot.as_deref()) {
                log::svcd(&format!(
                    "svcd: {}: discarding runtime record from another boot",
                    id
                ));
                runtime::remove(&id, &self.config.paths);
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
                    let since = Self::adopted_since(now, &record);
                    let last_exit = record.last_exit;
                    let failures = record.failures;
                    if let Some(slot) = self.slots.get_mut(&id) {
                        slot.adopt_finished(since, last_exit);
                    }
                    self.persisted
                        .insert(id.clone(), (State::Running, None, failures));
                } else {
                    runtime::remove(&id, &self.config.paths);
                }
                continue;
            };
            // Windows has no process to adopt: the job object took the tree
            // down with the daemon. Records are only cleaned up there.
            if cfg!(windows) {
                runtime::remove(&id, &self.config.paths);
                continue;
            }
            match runtime::pid_state(&record, Self::process_probe(pid)) {
                runtime::PidState::Gone => {
                    runtime::remove(&id, &self.config.paths);
                    continue;
                }
                runtime::PidState::Reused { expected, actual } => {
                    log::svcd(&format!(
                        "svcd: {}: pid {} was reused (starttime {} != {}); not adopting",
                        id, pid, actual, expected
                    ));
                    runtime::remove(&id, &self.config.paths);
                    continue;
                }
                runtime::PidState::Same | runtime::PidState::Unknown => {}
            }
            let since = Self::adopted_since(now, &record);
            let ready = anchored_mono_ms(now, record.ready_since);
            let failures = record.failures;
            let last_exit = record.last_exit;
            let state = record.state;
            let was_stopping = state == State::Stopping;
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.adopt(pid, state, since, ready, failures, last_exit, now.mono_ms);
                if was_stopping {
                    // Finish the interrupted stop the same way a live stop
                    // does: stop action first, then signals and the kill
                    // window. Otherwise the service's cleanup would be lost.
                    slot.request_stop(now.mono_ms);
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
        let directory = self.config.paths.run_dir.clone();
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
                if let Some(record) = runtime::load(id, &self.config.paths) {
                    if let Some(pid) = record.pid {
                        let state = runtime::pid_state(&record, Self::process_probe(pid));
                        if matches!(state, runtime::PidState::Same | runtime::PidState::Unknown) {
                            crate::platform::host::process::terminate_tree(pid);
                        }
                    }
                }
                runtime::remove(id, &self.config.paths);
                self.persisted.remove(id);
                log::svcd(&format!("svcd: dropped orphan runtime record {}", id));
            }
        }
    }

    /// Write the runtime record of every service whose (state, pid, failures)
    /// tuple changed since the last write. Stopped/failed services with no pid
    /// need no record: there is nothing to reconnect.
    fn persist_states(&mut self, now: Now) {
        let boot = self.boot_id.clone();
        for id in self.slots.keys().cloned().collect::<Vec<_>>() {
            // Read the same facet the status projection reads, and anchor it
            // with the kernel's one guarded conversion. A `since_ms` of 0 means
            // "never started", so it must stay absent on disk too.
            let live = self.slots[&id].machine.live();
            let (state, pid, failures) = (live.state, live.pid, live.failures);
            if self.persisted.get(&id) == Some(&(state, pid, failures)) {
                continue;
            }
            if pid.is_none() && matches!(state, State::Stopped | State::Failed) {
                runtime::remove(&id, &self.config.paths);
                self.persisted.remove(&id);
                continue;
            }
            let record = Record {
                boot: boot.clone(),
                state,
                pid,
                starttime: pid.and_then(crate::platform::host::process::start_time),
                since: anchored_secs(now, live.since_ms),
                ready_since: anchored_secs_opt(now, live.ready_since_ms),
                failures,
                last_exit: live.last_exit,
            };
            match runtime::save(&id, &record, &self.config.paths) {
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
            // hook) get an explicit poll closure from hook_pump.
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
            Some(reactor) => crate::wire::contract::stdio::Target::polled(reactor),
            None => crate::wire::contract::stdio::Target::threaded(),
        };
        for slot in self.slots.values_mut() {
            slot.set_stdio(self.stdio.clone());
        }
    }

    fn set_raw_hub(&mut self, raw_hub: Arc<crate::wire::contract::rawhub::RawHub>) {
        self.raw_hub = raw_hub;
        for slot in self.slots.values_mut() {
            slot.set_raw_hub(Some(self.raw_hub.clone()));
        }
    }

    /// Log a loop phase that ran past [`SLOW_LOOP_MS`].
    fn note_slow(&self, what: &str, started: Instant) {
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(SLOW_LOOP_MS) {
            log::svcd(&format!("svcd: {} held the loop for {:?}", what, elapsed));
        }
    }

    fn start_enabled(&mut self, clock: &Clock) {
        let started = Instant::now();
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
            let missing = self.registry.missing_dependencies(&id);
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
            self.note_slow("start_enabled", started);
            return;
        }
        let layers = match self.registry.start_layers(&eligible) {
            Ok(layers) => layers,
            Err(error) => {
                log::svcd(&format!("svcd: start enabled: {}", error));
                self.note_slow("start_enabled", started);
                return;
            }
        };
        for layer in layers {
            let deadline = clock
                .now()
                .mono_ms
                .saturating_add(supervisor::START_TIMEOUT_MS);
            if let Err(error) = self.start_layer(&layer, deadline, clock) {
                log::svcd(&format!("svcd: start enabled: {}", error));
            }
        }
        self.note_slow("start_enabled", started);
    }

    fn event_loop(&mut self, clock: &Clock) {
        loop {
            let disconnected = self.drain(clock);
            let now = clock.now();
            self.tick(now, clock);
            if self.shutdown || disconnected {
                break;
            }
            // Management requests and plugin stdout wake this immediately; the
            // bounded wait is only for shutdown and timer deadlines. A timer
            // that comes due sooner shortens the wait.
            self.wait_for_work(self.next_wait(now));
        }
        self.stop_all(clock);
    }

    /// Handle every queued management request, then report whether the control
    /// channel closed. Blocking requests (start / stop / reload) wait on the
    /// clock, so the drain takes the clock rather than a stale snapshot.
    fn drain(&mut self, clock: &Clock) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(incoming) => self.handle(clock, incoming),
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
    }

    /// One non-blocking supervisor pass: plugin stdio, the state machines, the
    /// periodic timers and the shutdown flag. Everything reads the same `now`
    /// snapshot, so a pass cannot mix two clock reads. The clock is only needed
    /// because a due rescan reconciles by blocking on start / stop.
    fn tick(&mut self, now: Now, clock: &Clock) {
        let started = Instant::now();
        self.pump(now);
        self.note_slow("pump", started);
        if self.rescan_interval_ms > 0 && now.mono_ms >= self.next_rescan_ms {
            self.next_rescan_ms = now.mono_ms.saturating_add(self.rescan_interval_ms);
            let started = Instant::now();
            self.rescan_if_changed(clock);
            self.note_slow("rescan", started);
        }
        if self.temp_cleanup_interval_ms > 0 && now.mono_ms >= self.next_temp_cleanup_ms {
            self.next_temp_cleanup_ms = now.mono_ms.saturating_add(self.temp_cleanup_interval_ms);
            let started = Instant::now();
            match crate::platform::storage::cleanup(&self.config.paths, self.temp_retention) {
                Ok(0) => {}
                Ok(removed) => {
                    log::svcd(&format!("svcd: temp cleanup removed {} file(s)", removed))
                }
                Err(error) => log::svcd(&format!("svcd: temp cleanup failed: {}", error)),
            }
            self.note_slow("temp cleanup", started);
        }
        if crate::platform::host::signal::requested() {
            self.shutdown = true;
        }
    }

    /// How long the loop may wait before the next timer is due, capped by the
    /// poll tick so shutdown and the reactor are serviced even with no timers.
    fn next_wait(&self, now: Now) -> Duration {
        let mut wait = POLL_WAIT;
        for (interval, next) in [
            (self.rescan_interval_ms, self.next_rescan_ms),
            (self.temp_cleanup_interval_ms, self.next_temp_cleanup_ms),
        ] {
            if interval > 0 {
                wait = wait.min(Duration::from_millis(next.saturating_sub(now.mono_ms)));
            }
        }
        wait
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

    fn handle(&mut self, clock: &Clock, incoming: channel::PendingCall) {
        let started = Instant::now();
        let request = match rpc::parse(&incoming.line) {
            Ok(request) => request,
            Err(response) => {
                incoming.reply.send(rpc::to_line(&response));
                return;
            }
        };
        let id = request.id.clone();
        let method = request.method.clone();
        if incoming.endpoint == channel::Endpoint::User
            && channel::Endpoint::for_method(&method) == channel::Endpoint::Admin
        {
            let response =
                rpc::err_data(id, -32000, "unauthorized", Problem::Internal.error_data());
            incoming.reply.send(rpc::to_line(&response));
            return;
        }
        let call = match control::parse(request) {
            Ok(call) => call,
            Err(response) => {
                incoming.reply.send(rpc::to_line(&response));
                return;
            }
        };
        match call.command {
            control::Command::ToolCall(tool) => {
                self.handle_tool_call(clock, call.id, tool, incoming.reply)
            }
            command => {
                let response = self.dispatch(clock, call.id, command);
                incoming.reply.send(rpc::to_line(&response));
            }
        }
        self.note_slow(&format!("request {}", method), started);
    }

    fn dispatch(
        &mut self,
        clock: &Clock,
        id: Option<Value>,
        command: control::Command,
    ) -> rpc::Response {
        let now = clock.now();
        match command {
            control::Command::Ping => rpc::ok(id, json!({"ok": true})),
            control::Command::Compat => rpc::ok(
                id,
                serde_json::to_value(compat::detect(&self.config.paths, MAX_SCHEMA, MAX_PROTOCOL))
                    .unwrap_or(Value::Null),
            ),
            control::Command::Shutdown => {
                self.shutdown = true;
                rpc::ok(id, json!({"ok": true}))
            }
            control::Command::ToolsPage { cursor, limit } => {
                match self.tools_page(cursor.as_deref(), limit) {
                    Ok(value) => rpc::ok(id, value),
                    Err(error) => problem_response(id, -32602, error),
                }
            }
            control::Command::Version => rpc::ok(id, json!({ "version": self.catalog_version })),
            control::Command::List => rpc::ok(id, json!({"services": self.statuses(now)})),
            control::Command::Status { service, probe } => {
                match self.status_for_id(&service, now) {
                    Some(mut status) => {
                        if probe {
                            self.probe_status(&service, &mut status);
                        }
                        rpc::ok(id, status_json(status))
                    }
                    None => rpc::err_data(
                        id,
                        -32001,
                        &format!("unknown service: {}", service),
                        Problem::UnknownService.error_data(),
                    ),
                }
            }
            control::Command::Start { service } => {
                status_response(id, -32003, self.start_service(&service, clock))
            }
            control::Command::Stop { service } => {
                status_response(id, -32004, self.stop_service(&service, clock))
            }
            control::Command::Restart { service } => {
                status_response(id, -32010, self.restart_service(&service, clock))
            }
            control::Command::Enable { service } => {
                status_response(id, -32005, self.set_enabled(&service, true, clock))
            }
            control::Command::Disable { service } => {
                status_response(id, -32005, self.set_enabled(&service, false, clock))
            }
            control::Command::Reload => match self.reload(clock) {
                Ok(summary) => rpc::ok(id, summary),
                Err(error) => problem_response(id, -32009, error),
            },
            control::Command::Install(install) => match self.install_plugin(install, clock) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => problem_response(id, -32006, error),
            },
            control::Command::InstallStaged(install) => match self.install_staged(install, clock) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => problem_response(id, -32006, error),
            },
            control::Command::Uninstall { service, purge } => {
                match self.uninstall_plugin(&service, purge, clock) {
                    Ok(()) => rpc::ok(id, json!({ "ok": true })),
                    Err(error) => problem_response(id, -32007, error),
                }
            }
            control::Command::AuthStatus => match self.auth_status() {
                Ok(status) => rpc::ok(id, auth_json(&status)),
                Err(error) => problem_response(id, -32008, error),
            },
            control::Command::AuthPasswd { password } => match self.auth_passwd(&password) {
                Ok(status) => rpc::ok(id, auth_json(&status)),
                Err(error) => problem_response(id, -32008, error),
            },
            // handle() routes streaming tool calls to the reply sink; a direct
            // caller has no sink, so refuse instead of dropping the call.
            control::Command::ToolCall(_) => rpc::err_data(
                id,
                -32603,
                "tools/call needs a control reply sink",
                Problem::Internal.error_data(),
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
        let path =
            self.config.settings_path.as_deref().ok_or_else(|| {
                ProblemError::new(Problem::Internal, "svcd.conf path is unavailable")
            })?;
        let hash = crate::platform::auth::hash_password(password)
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

    fn statuses(&self, now: Now) -> Vec<Status> {
        self.registry
            .managed_values()
            .map(|service| self.status_for(service, now))
            .collect()
    }

    fn status_for_id(&self, id: &str, now: Now) -> Option<Status> {
        self.registry
            .get(id)
            .map(|service| self.status_for(service, now))
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
        mcp_tools::tools(&self.registry, |id| self.is_live(id))
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

    fn tools_page(
        &mut self,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, ProblemError> {
        let version = self.catalog_version;
        let page =
            catalog::page(self.tool_list_cached(), version, cursor, limit).map_err(|error| {
                let problem = match error {
                    crate::face::catalog::PageError::InvalidCursor => Problem::Invalid,
                    // Both today fall through to 500; keep that projection.
                    crate::face::catalog::PageError::VersionMismatch
                    | crate::face::catalog::PageError::PastEnd => Problem::Internal,
                };
                ProblemError::new(problem, error.to_string())
            })?;
        // The control channel carries a face-neutral page; the MCP face renders
        // the protocol field `nextCursor` (ADR-0025: protocol projection is the
        // face's job). Keep `next` neutral here so the composition root never
        // spells an MCP key.
        let mut result = json!({ "tools": page.tools });
        if let Some(cursor) = page.next_cursor {
            result["next"] = json!(cursor);
        }
        Ok(result)
    }

    /// Handle tools/call. Plugin calls are deferred: the plugin response arrives
    /// on the slot pump, so the control connection stays open without blocking
    /// the daemon's event loop. A start tool is synchronous because starting is
    /// defined as synchronous in the service contract.
    fn handle_tool_call(
        &mut self,
        clock: &Clock,
        id: Option<Value>,
        call: control::ToolCall,
        reply: crate::wire::control::channel::Reply,
    ) {
        let name = call.name;
        let mut arguments = call.arguments;

        // Only a known plugin's <id>.start is the lifecycle tool. A capability
        // named "foo.start" falls through to the capability lookup below.
        let start_id = name.strip_suffix(".start").filter(|service_id| {
            self.registry
                .get(service_id)
                .map(|service| {
                    service.kind == Kind::Plugin || !service.contract.surface().is_empty()
                })
                .unwrap_or(false)
        });
        if let Some(service_id) = start_id {
            let response = match self.start_service(service_id, clock) {
                Ok(status) => mcp_tools::tool_result(id, status_json(status).to_string(), false),
                Err(error) => mcp_tools::tool_error(id, error.detail()),
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
                mcp_tools::tool_result(id, format!("service {} is not running", service_id), true),
            );
            return;
        }

        if let Err(message) =
            crate::platform::storage::resolve_args(&self.config.paths, &mut arguments)
        {
            send(
                &reply,
                mcp_tools::tool_error(id, &format!("cannot resolve blob argument: {}", message)),
            );
            return;
        }
        let progress_token = call.progress;
        let mut invocation =
            Invocation::new(capability, arguments, self.config.limits.default_timeout);
        invocation.progress = progress_token;

        if let Some(spec) = builtin {
            let launchers = self.launchers.clone();
            let contract = service.contract.clone();
            let mut instance = match launchers.launch(contract, &spec) {
                Ok(instance) => instance,
                Err(error) => {
                    reply.send(mcp_tools::project_builtin_outcome(
                        id,
                        Err(error.into_failure()),
                    ));
                    return;
                }
            };
            let done = projected_done(
                reply.clone(),
                id.clone(),
                "stream replies are not supported by the builtin adapter",
                mcp_tools::project_builtin_outcome,
            );
            if let Err(rejection) = instance.call(invocation, done) {
                reply.send(rpc::to_line(&mcp_tools::project_rejection(id, rejection)));
            }
            return;
        }

        let Some(slot) = self.slots.get_mut(service_id) else {
            send(
                &reply,
                mcp_tools::tool_result(id, format!("service {} has no runtime", service_id), true),
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
                reply.send(mcp_tools::project_slot_outcome(id, Err(failure)));
                return;
            }
        };
        let done = projected_done(
            reply.clone(),
            id.clone(),
            "stream replies are not wired through SlotWire",
            mcp_tools::project_slot_outcome,
        );
        if let Err(rejection) = instance.call(invocation, done) {
            reply.send(rpc::to_line(&mcp_tools::project_rejection(id, rejection)));
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
        let mut poll = self.hook_pump();
        status.details.probe = Some(run_status_hook(id, &hook, &self.config.paths, &mut poll));
    }

    /// A one-pass poll for a synchronous hook wait. The closure owns the
    /// reactor handle, so running the hook with a mutable borrow of self
    /// cannot re-borrow the poll set through a hidden thread-local.
    #[cfg(unix)]
    fn hook_pump(&self) -> impl FnMut() + 'static {
        let reactor = self.reactor.clone();
        move || {
            if let Some(reactor) = &reactor {
                reactor.poll_once(Duration::ZERO);
            }
        }
    }

    #[cfg(not(unix))]
    fn hook_pump(&self) -> impl FnMut() + 'static {
        || {}
    }

    fn status_for(&self, service: &Service, now: Now) -> Status {
        if service.id == RESERVED_ID {
            return Status::builtin(service);
        }
        match self.slots.get(&service.id) {
            Some(slot) => Status::live(service, &slot.machine.live(), now),
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
    fn rescan_if_changed(&mut self, clock: &Clock) {
        let fingerprint = services_fingerprint(&self.config.paths.services_dir);
        if fingerprint == self.services_fingerprint {
            return;
        }
        self.services_fingerprint = fingerprint;
        match self.reconcile_definitions(clock) {
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

    fn reload(&mut self, clock: &Clock) -> Result<Value, ProblemError> {
        let summary = self.reconcile_definitions(clock)?;
        self.services_fingerprint = services_fingerprint(&self.config.paths.services_dir);
        Ok(summary)
    }

    /// Re-scan `services.d` plus the plugin store and reconcile the registry.
    /// Running services keep running (a changed exec applies at the next start);
    /// definitions that disappeared are stopped and forgotten. Shared by
    /// `svc reload`, the periodic rescan, and boot recovery.
    fn reconcile_definitions(&mut self, clock: &Clock) -> Result<Value, ProblemError> {
        let loaded =
            definition::load_services(&self.config.paths.services_dir, &self.config.limits);
        for issue in &loaded.issues {
            log::svcd(&format!("svcd: reload: warning: {}", issue));
        }
        let (plugins, warnings) = plugin::scan(&self.config.paths, &self.config.limits);
        for warning in warnings {
            log::svcd(&format!("svcd: reload: plugin: {}", warning));
        }
        // Core definitions win; an installed plugin still lands in `desired` so
        // the stale sweep below does not unregister it on every reload.
        let desired = reconcile::merge_desired(loaded.registry.managed_values().cloned(), plugins);

        let mut removed = Vec::new();
        let stale = reconcile::stale_ids(
            self.registry
                .managed_values()
                .map(|service| service.id.as_str()),
            &desired,
        );
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
                let _ = self.stop_service(&id, clock);
            }
            self.slots.remove(&id);
            self.registry.remove(&id);
            runtime::remove(&id, &self.config.paths);
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
                if let Err(error) = self.start_service(id, clock) {
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
            let old = self.registry.get(&id).cloned();
            match reconcile::action_for(old.as_ref(), &service) {
                reconcile::Action::Skip => continue,
                reconcile::Action::Add => {
                    let exec = service.exec.clone();
                    let lifecycle = service.lifecycle.clone();
                    let restart = service.restart;
                    let protocol = service.protocol;
                    let plugin_only = service.kind == Kind::Plugin;
                    let remain_after_exit = service.remain_after_exit;
                    if let Some(exec) = exec {
                        let mut slot = Slot::new(
                            id.clone(),
                            exec,
                            lifecycle,
                            restart,
                            self.config.paths.clone(),
                            self.config.limits.clone(),
                        );
                        slot.set_protocol(protocol);
                        slot.set_protocol_only(plugin_only);
                        slot.set_remain_after_exit(remain_after_exit);
                        slot.set_wake(self.wake.clone());
                        slot.set_stdio(self.stdio.clone());
                        slot.set_raw_hub(Some(self.raw_hub.clone()));
                        configure_slot(&mut slot, &service, &self.config.limits, &self.bus);
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
                reconcile::Action::Update => {
                    // Plugins are managed through install/uninstall RPCs; the
                    // pure classifier already returned Skip for those.
                    let old = old.expect("update implies an existing definition");
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

    fn start_service(&mut self, id: &str, clock: &Clock) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id, clock.now()).expect("builtin"));
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
                &compat::detect(&self.config.paths, MAX_SCHEMA, MAX_PROTOCOL),
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
        let now = clock.now();
        if let Some(slot) = self.slots.get_mut(id) {
            if slot.machine.state == State::Running
                && slot.machine.pid.is_none()
                && slot.machine.remain_after_exit
            {
                slot.apply(Event::Start, now.mono_ms);
            }
        }

        let layers = self.registry.start_layers(&[id.to_string()])?;
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
        let deadline = now.mono_ms.saturating_add(supervisor::START_TIMEOUT_MS);
        for layer in layers {
            self.start_layer(&layer, deadline, clock)?;
        }

        self.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    fn start_layer(
        &mut self,
        ids: &[String],
        deadline: u64,
        clock: &Clock,
    ) -> Result<(), ProblemError> {
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
            if self.slots[id].machine.state == State::Starting {
                log::svcd(&format!(
                    "svcd: {}: start requested while already Starting; no new spawn for this call",
                    id
                ));
            }
            let now = clock.now().mono_ms;
            self.slots
                .get_mut(id)
                .expect("slot")
                .apply(Event::Start, now);
        }

        loop {
            self.pump(clock.now());
            let mut all_settled = true;
            for id in &pending {
                let slot = &self.slots[id];
                let state = slot.machine.state;
                match start::start_step(state, slot.is_channel_peer()) {
                    start::Step::Settled => {}
                    start::Step::Finished => {
                        // It reached ready and then exited cleanly before this
                        // pass looked (`ready: exec` one-shots do exactly that).
                        // Waiting for Running burns the whole window and reports
                        // a timeout for a start that did succeed.
                        log::svcd(&format!(
                            "svcd: {}: exited cleanly during start; the start is complete",
                            id
                        ));
                    }
                    start::Step::Waiting => all_settled = false,
                    start::Step::Failed => {
                        return Err(ProblemError::new(
                            Problem::Internal,
                            format!("service {} failed to start", id),
                        ));
                    }
                    start::Step::ChannelLost => {
                        // It answered ready and then left. The channel callers
                        // depend on is gone, so this start produced nothing
                        // usable even though the exit itself was clean.
                        let code = slot
                            .machine
                            .last_exit
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "unknown".into());
                        log::svcd(&format!(
                            "svcd: {}: exited during start (exit code {}); the start failed",
                            id, code
                        ));
                        return Err(ProblemError::new(
                            Problem::Internal,
                            format!("service {} exited during start (exit code {})", id, code),
                        ));
                    }
                }
            }
            if all_settled {
                return Ok(());
            }
            if clock.now().mono_ms >= deadline {
                for id in &pending {
                    log::svcd(&format!(
                        "svcd: {}: start timed out waiting for ready ({})",
                        id,
                        self.slots[id].handshake_snapshot()
                    ));
                }
                return Err(ProblemError::new(
                    Problem::Timeout,
                    format!("start timed out: {}", pending.join(", ")),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn install_plugin(
        &mut self,
        install: control::Install,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        let path = install.path.as_str();
        let strategy = install.strategy;
        let allow_hooks = install.allow_hooks;

        // Everything that can reject the package runs before anything is
        // stopped or written: a bad package must leave the installed version
        // untouched. `plugin::verify` is the same preflight `plugin verify`
        // exposes, so the two answers cannot drift apart.
        let verified = plugin::verify(
            &self.config.paths,
            Path::new(path),
            self.config.settings.allow_insecure_install,
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
            self.stop_service(&id, clock)?;
        }
        let still_running = running(&self.slots);
        let mut poll = self.hook_pump();
        let service = match plugin::install_package(
            &self.config.paths,
            &self.config.limits,
            &package,
            strategy,
            allow_hooks,
            host_interpreter.clone(),
            &mut poll,
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
                plugin::install_package(
                    &self.config.paths,
                    &self.config.limits,
                    &package,
                    Strategy::Replace,
                    allow_hooks,
                    host_interpreter,
                    &mut poll,
                )
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
                let mut slot = Slot::new(
                    id.clone(),
                    exec,
                    service.lifecycle.clone(),
                    service.restart,
                    self.config.paths.clone(),
                    self.config.limits.clone(),
                );
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
                configure_slot(&mut slot, &service, &self.config.limits, &self.bus);
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
        self.status_for_id(&id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    fn install_staged(
        &mut self,
        install: control::Install,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
        let path = install.path.as_str();
        let strategy = install.strategy;
        let allow_hooks = install.allow_hooks;

        let staging = Path::new(path).canonicalize().map_err(|error| {
            ProblemError::new(
                Problem::Internal,
                format!("staging path {}: {}", path, error),
            )
        })?;
        let plugins = self
            .config
            .paths
            .plugins_dir
            .canonicalize()
            .map_err(|error| {
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

        let staged = package::load_staged(
            &staging,
            self.config.settings.allow_insecure_install,
            &self.config.paths.trust_dir,
        )?;
        let id = staged.manifest.name.clone();
        let version = staged.manifest.version.clone();
        if let Some(requested) = install.expected_id.as_deref() {
            if requested != id {
                return Err(ProblemError::new(
                    Problem::Internal,
                    "staged id does not match the request",
                ));
            }
        }
        if let Some(requested) = install.expected_version.as_deref() {
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
        let host_interpreter = plugin::prepare_staged(&staged, allow_hooks)?;
        let signature_verified = staged.signature_verified;

        let running = |slots: &BTreeMap<String, Slot>| {
            slots
                .get(&id)
                .map(|slot| !matches!(slot.machine.state, State::Stopped | State::Failed))
                .unwrap_or(false)
        };
        if running(&self.slots) && strategy == Strategy::Replace {
            self.stop_service(&id, clock)?;
        }
        let still_running = running(&self.slots);
        let mut poll = self.hook_pump();
        let service = plugin::install_staged(
            &self.config.paths,
            &self.config.limits,
            staged,
            strategy,
            allow_hooks,
            host_interpreter,
            &mut poll,
        )?;
        if let Err(error) = service.carrier() {
            return Err(ProblemError::new(
                Problem::Internal,
                format!("plugin {}: {}", id, error),
            ));
        }
        if !still_running {
            self.slots.remove(&id);
            if let Some(exec) = service.exec.clone() {
                let mut slot = Slot::new(
                    id.clone(),
                    exec,
                    service.lifecycle.clone(),
                    service.restart,
                    self.config.paths.clone(),
                    self.config.limits.clone(),
                );
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
                configure_slot(&mut slot, &service, &self.config.limits, &self.bus);
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
        self.status_for_id(&id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    fn uninstall_plugin(
        &mut self,
        id: &str,
        purge: bool,
        clock: &Clock,
    ) -> Result<(), ProblemError> {
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
            self.stop_service(id, clock)?;
        }
        let mut poll = self.hook_pump();
        plugin::uninstall(
            &self.config.paths,
            &self.config.limits,
            id,
            purge,
            &mut poll,
        )?;
        self.slots.remove(id);
        self.registry.remove(id);
        runtime::remove(id, &self.config.paths);
        self.persisted.remove(id);
        self.bump_catalog();
        if purge {
            plugin::remove_logs(&self.config.paths, id);
            #[cfg(feature = "http")]
            self.bus.forget(id);
        }
        log::svcd(&format!("svcd: plugin {} uninstalled", id));
        Ok(())
    }

    fn set_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        clock: &Clock,
    ) -> Result<Status, ProblemError> {
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
            Kind::Plugin => (
                plugin::plugin_dir(&self.config.paths, id).join("enabled"),
                enabled,
            ),
            Kind::Core => (
                self.config
                    .paths
                    .services_dir
                    .join(format!("{}.disabled", id)),
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
        self.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    /// `svc restart` = stop then start, on one RPC so no other caller can slip
    /// in between. A finished one-shot is stopped (settles immediately) and
    /// then runs again.
    fn restart_service(&mut self, id: &str, clock: &Clock) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id, clock.now()).expect("builtin"));
        }
        self.stop_service(id, clock)?;
        self.start_service(id, clock)
    }

    fn stop_service(&mut self, id: &str, clock: &Clock) -> Result<Status, ProblemError> {
        if self.registry.get(id).is_none() {
            return Err(ProblemError::new(
                Problem::UnknownService,
                format!("unknown service: {}", id),
            ));
        }
        if id == RESERVED_ID {
            return Ok(self.status_for_id(id, clock.now()).expect("builtin"));
        }
        let Some(state) = self.slots.get(id).map(|slot| slot.machine.state) else {
            return self.status_for_id(id, clock.now()).ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            });
        };

        let now = clock.now();
        self.slots
            .get_mut(id)
            .expect("slot")
            .request_stop(now.mono_ms);
        if matches!(state, State::Stopped | State::Failed) {
            return self.status_for_id(id, clock.now()).ok_or_else(|| {
                ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
            });
        }
        let deadline = now
            .mono_ms
            .saturating_add(supervisor::STOP_TIMEOUT_MS)
            .saturating_add(supervisor::KILL_TIMEOUT_MS)
            .saturating_add(1_000);
        loop {
            self.pump(clock.now());
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
            if clock.now().mono_ms >= deadline {
                return Err(ProblemError::new(
                    Problem::Timeout,
                    format!("stop timed out: {}", id),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.status_for_id(id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownService, format!("unknown service: {}", id))
        })
    }

    fn pump(&mut self, now: Now) {
        // Plugin stdio belongs to this loop: one non-blocking pass before every
        // pump keeps plugin pipes moving inside a synchronous start/stop wait.
        // The reader-thread target needs no equivalent — its threads deliver
        // into the slot channels on their own.
        self.stdio.pump();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        let mut changed = false;
        for id in ids {
            let mut slot = self.slots.remove(&id).expect("slot");
            let before = slot.machine.state;
            slot.pump(now.mono_ms);
            if slot.machine.state != before {
                changed = true;
            }
            self.slots.insert(id, slot);
        }
        if changed {
            self.bump_catalog();
        }
        self.persist_states(now);
    }

    fn stop_all(&mut self, clock: &Clock) {
        let started = Instant::now();
        let ids: Vec<String> = self.slots.keys().cloned().collect();
        let layers = self
            .registry
            .start_layers(&ids)
            .unwrap_or_else(|_| vec![ids]);
        let global_deadline = clock
            .now()
            .mono_ms
            .saturating_add(supervisor::SHUTDOWN_BUDGET_MS);
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
                let now = clock.now().mono_ms;
                self.slots.get_mut(id).expect("slot").request_stop(now);
            }
            let deadline = clock
                .now()
                .mono_ms
                .saturating_add(supervisor::STOP_TIMEOUT_MS)
                .saturating_add(supervisor::KILL_TIMEOUT_MS)
                .min(global_deadline);
            loop {
                self.pump(clock.now());
                if active.iter().all(|id| {
                    matches!(self.slots[id].machine.state, State::Stopped | State::Failed)
                }) {
                    break;
                }
                if clock.now().mono_ms >= deadline {
                    if clock.now().mono_ms >= global_deadline {
                        for slot in self.slots.values_mut() {
                            if !matches!(slot.machine.state, State::Stopped | State::Failed) {
                                slot.signal_force("daemon shutdown past its global budget");
                            }
                        }
                        break 'layers;
                    }
                    for id in &active {
                        if !matches!(self.slots[id].machine.state, State::Stopped | State::Failed) {
                            if let Some(slot) = self.slots.get_mut(id) {
                                slot.signal_force("daemon shutdown per-layer timeout");
                            }
                        }
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        self.note_slow("stop_all", started);
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
                Problem::Internal.error_data(),
            ),
        },
        Err(error) => problem_response(id, error_code, error),
    }
}

/// One management error -> one JSON-RPC frame. Funnelling every dispatch arm
/// through here keeps the `data.kind` and the code together.
fn problem_response(id: Option<Value>, code: i32, error: ProblemError) -> rpc::Response {
    rpc::err_data(id, code, error.detail(), error.problem.error_data())
}

fn status_json(status: Status) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

/// Build the completion sink for one tools/call: a stream reply is an
/// implementation error, and the final value/failure is projected by the face.
fn projected_done(
    reply: crate::wire::control::channel::Reply,
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
        reply.send(project(external_id, outcome));
    })
}

/// Give a slot its contract and the face-side projections. The slot itself no
/// longer knows MCP or the UI bus.
fn configure_slot(
    slot: &mut Slot,
    service: &Service,
    limits: &crate::platform::limits::Limits,
    bus: &Arc<EventBus>,
) {
    slot.set_contract(service.contract.clone());
    slot.set_call_timeout(limits.default_timeout);
    let _id = service.id.clone();
    let bus = bus.clone();
    slot.set_notifier(Arc::new(move |method, params| {
        #[cfg(feature = "http")]
        bus.publish(&_id, method, params);
        #[cfg(not(feature = "http"))]
        {
            let _ = (&bus, method, params);
        }
    }));
}

fn send(reply: &crate::wire::control::channel::Reply, response: rpc::Response) {
    reply.send(rpc::to_line(&response));
}

fn unknown_tool(id: &Option<Value>, name: &str) -> rpc::Response {
    rpc::err(id.clone(), -32602, &format!("unknown tool: {}", name))
}

/// The status action is a detail supplement, not a health gate: its stdout
/// becomes the `probe` text (capped), and a non-zero exit only appends the
/// status word. Capture goes through a temp file so a hook that leaves a child
/// holding stdout cannot block the daemon's event loop.
fn run_status_hook(id: &str, hook: &Hook, paths: &Paths, poll: &mut dyn FnMut()) -> String {
    let output_path = status_output_path(id, paths);
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
    let mut command = hook_command(id, "status", hook, paths);
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
            crate::supervise::ops::hook::kill(&mut child);
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

fn status_output_path(id: &str, paths: &Paths) -> PathBuf {
    let directory = paths.temp_dir.clone();
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

fn auth_json(status: &LanStatus) -> Value {
    json!({
        "bind": status.bind.to_string(),
        "requestedBind": status.requested_bind.to_string(),
        "authRequired": status.require_auth,
        "passwordSet": status.password_set,
    })
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "http"))]
mod http_assembly_tests {
    use super::{default_bind, resolve_bind};

    #[test]
    fn bind_precedence_is_env_then_config_then_default() {
        let a: std::net::SocketAddr = "127.0.0.1:1234".parse().expect("addr");
        let b: std::net::SocketAddr = "0.0.0.0:5678".parse().expect("addr");
        assert_eq!(resolve_bind(Some("127.0.0.1:1234"), Some(b)), a);
        assert_eq!(resolve_bind(Some("not-an-addr"), Some(b)), b);
        assert_eq!(resolve_bind(None, Some(b)), b);
        assert_eq!(resolve_bind(None, None), default_bind());
    }
}
