use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
#[cfg(test)]
use std::time::SystemTime;
use std::time::{Duration, Instant};

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
use crate::kernel::contract::{CapabilityName, Invocation, Reply, ReplySink};
use crate::kernel::failure::{Class, Failure, Problem, ProblemError, Retry};
use crate::kernel::projection::Status;
use crate::kernel::registry::{Kind, Registry};
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::Strategy;
use crate::platform::clock::Clock;
use crate::platform::compat;
use crate::platform::config::Config;
use crate::platform::host::wake::Wake;
use crate::platform::log;
#[cfg(test)]
use crate::platform::paths::Paths;
#[cfg(feature = "http")]
use crate::platform::settings;
#[cfg(feature = "http")]
use crate::server::auth::lan::Lan;
use crate::server::auth::lan::LanStatus;
use crate::supervise::model::definition;
use crate::supervise::ops::factory::Notifier;
use crate::supervise::ops::host::Supervisor;
use crate::supervise::ops::invoke::{self, Outcome};
#[cfg(unix)]
use crate::wire::contract::reactor::Reactor;
use crate::wire::control::channel;
use crate::wire::message::rpc;

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
    daemon.host.set_raw_hub(raw_hub.clone());
    daemon.host.set_wake(wake);
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
        daemon.host.set_reactor(reactor);
    }
    let clock = Clock::boot();
    daemon.host.adopt_runtime(clock.now());
    daemon.host.sweep_runtime_records();
    crate::platform::host::signal::install();
    daemon.host.start_enabled(&clock);
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
    /// The supervision state and its loop-facing interface (L3).
    host: Supervisor,
    rx: Receiver<channel::PendingCall>,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    api: Option<Arc<Api>>,
    shutdown: bool,
    catalog_version: u64,
    catalog_cache: Option<(u64, Vec<Value>)>,
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
        let notifier: Notifier = {
            let bus = bus.clone();
            Arc::new(move |id: &str, method: &str, params: &Value| {
                #[cfg(feature = "http")]
                bus.publish(id, method, params);
                #[cfg(not(feature = "http"))]
                {
                    let _ = (id, method, params);
                }
            })
        };
        let host = Supervisor::new(config.clone(), registry, raw_hub, notifier);
        Daemon {
            config,
            host,
            rx,
            api,
            shutdown: false,
            catalog_version: 1,
            catalog_cache: None,
            bus,
        }
    }

    fn event_loop(&mut self, clock: &Clock) {
        loop {
            let disconnected = self.drain(clock);
            let now = clock.now();
            self.host.tick(now, clock);
            self.sync_catalog();
            self.shutdown |= crate::platform::host::signal::requested();
            if self.shutdown || disconnected {
                break;
            }
            // Management requests and plugin stdout wake this immediately; the
            // bounded wait is only for shutdown and timer deadlines. A timer
            // that comes due sooner shortens the wait.
            self.host.wait_for_work(self.host.next_wait(now));
        }
        self.host.stop_all(clock);
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
        self.sync_catalog();
        log::note_slow(&format!("request {}", method), started);
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
            control::Command::List => rpc::ok(id, json!({"services": self.host.statuses(now)})),
            control::Command::Status { service, probe } => {
                match self.host.status_for_id(&service, now) {
                    Some(mut status) => {
                        if probe {
                            self.host.probe_status(&service, &mut status);
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
                status_response(id, -32003, self.host.start_service(&service, clock))
            }
            control::Command::Stop { service } => {
                status_response(id, -32004, self.host.stop_service(&service, clock))
            }
            control::Command::Restart { service } => {
                status_response(id, -32010, self.host.restart_service(&service, clock))
            }
            control::Command::Enable { service } => {
                status_response(id, -32005, self.host.set_enabled(&service, true, clock))
            }
            control::Command::Disable { service } => {
                status_response(id, -32005, self.host.set_enabled(&service, false, clock))
            }
            control::Command::Reload => match self.host.reload(clock) {
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

    fn tool_list(&self) -> Vec<Value> {
        mcp_tools::tools(&self.host.registry, |id| self.host.is_live(id))
    }

    /// Invalidate the cached tool projection. The cache is keyed by the
    /// catalog version so a page is always cut from one stable snapshot.
    /// The supervisor reports a moved projection; the cache lives here.
    fn sync_catalog(&mut self) {
        if self.host.take_catalog_dirty() {
            self.bump_catalog();
        }
    }

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
            self.host
                .registry
                .get(service_id)
                .map(|service| {
                    service.kind == Kind::Plugin || !service.contract.surface().is_empty()
                })
                .unwrap_or(false)
        });
        if let Some(service_id) = start_id {
            let response = match self.host.start_service(service_id, clock) {
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
        let Some(service) = self.host.service(service_id) else {
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
            || self.host.slots.contains_key(service_id);
        if !known || !callable {
            send(&reply, unknown_tool(&id, &name));
            return;
        }
        if builtin.is_some() && !self.host.is_live(service_id) {
            send(
                &reply,
                mcp_tools::tool_result(id, format!("service {} is not running", service_id), true),
            );
            return;
        }
        let service = service.clone();

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

        let builtin_done = projected_done(
            reply.clone(),
            id.clone(),
            "stream replies are not supported by the builtin adapter",
            mcp_tools::project_builtin_outcome,
        );
        let slot_done = projected_done(
            reply.clone(),
            id.clone(),
            "stream replies are not wired through SlotWire",
            mcp_tools::project_slot_outcome,
        );
        match invoke::call(
            &mut self.host,
            &service,
            invocation,
            &reply,
            builtin_done,
            slot_done,
        ) {
            Outcome::Started => {}
            Outcome::NoRuntime => send(
                &reply,
                mcp_tools::tool_result(id, format!("service {} has no runtime", service_id), true),
            ),
            Outcome::BuiltinFailed(failure) => {
                reply.send(mcp_tools::project_builtin_outcome(id, Err(failure)))
            }
            Outcome::SlotFailed(failure) => {
                reply.send(mcp_tools::project_slot_outcome(id, Err(failure)))
            }
            Outcome::Rejected(rejection) => {
                reply.send(rpc::to_line(&mcp_tools::project_rejection(id, rejection)))
            }
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
        if let Some(existing) = self.host.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::CoreService,
                    format!("id is a core service: {}", id),
                ));
            }
        }

        if self.host.is_running(&id) && strategy == Strategy::Replace {
            self.host.stop_service(&id, clock)?;
        }
        let still_running = self.host.is_running(&id);
        let mut poll = self.host.hook_pump();
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
        self.host
            .install(service, still_running)
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
        self.host.status_for_id(&id, clock.now()).ok_or_else(|| {
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
        if let Some(existing) = self.host.registry.get(&id) {
            if existing.kind != Kind::Plugin {
                return Err(ProblemError::new(
                    Problem::CoreService,
                    format!("id is a core service: {}", id),
                ));
            }
        }
        let host_interpreter = plugin::prepare_staged(&staged, allow_hooks)?;
        let signature_verified = staged.signature_verified;

        if self.host.is_running(&id) && strategy == Strategy::Replace {
            self.host.stop_service(&id, clock)?;
        }
        let still_running = self.host.is_running(&id);
        let mut poll = self.host.hook_pump();
        let service = plugin::install_staged(
            &self.config.paths,
            &self.config.limits,
            staged,
            strategy,
            allow_hooks,
            host_interpreter,
            &mut poll,
        )?;
        self.host
            .install(service, still_running)
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
        self.host.status_for_id(&id, clock.now()).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })
    }

    fn uninstall_plugin(
        &mut self,
        id: &str,
        purge: bool,
        clock: &Clock,
    ) -> Result<(), ProblemError> {
        let service = self.host.registry.get(id).ok_or_else(|| {
            ProblemError::new(Problem::UnknownPlugin, format!("unknown plugin: {}", id))
        })?;
        if service.kind != Kind::Plugin {
            return Err(ProblemError::new(
                Problem::NotAPlugin,
                format!("not a plugin: {}", id),
            ));
        }
        if self.host.is_running(id) {
            self.host.stop_service(id, clock)?;
        }
        let mut poll = self.host.hook_pump();
        plugin::uninstall(
            &self.config.paths,
            &self.config.limits,
            id,
            purge,
            &mut poll,
        )?;
        self.host.forget(id);
        self.bump_catalog();
        if purge {
            plugin::remove_logs(&self.config.paths, id);
            #[cfg(feature = "http")]
            self.bus.forget(id);
        }
        log::svcd(&format!("svcd: plugin {} uninstalled", id));
        Ok(())
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

fn send(reply: &crate::wire::control::channel::Reply, response: rpc::Response) {
    reply.send(rpc::to_line(&response));
}

fn unknown_tool(id: &Option<Value>, name: &str) -> rpc::Response {
    rpc::err(id.clone(), -32602, &format!("unknown tool: {}", name))
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
