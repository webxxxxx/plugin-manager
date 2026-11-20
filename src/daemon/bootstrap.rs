use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::face::uichannel::EventBus;
use crate::manage as plugin;
use crate::platform::clock::Clock;
use crate::platform::config::Config;
use crate::platform::host::wake::Wake;
use crate::platform::log;
use crate::supervise::model::definition;
#[cfg(unix)]
use crate::wire::contract::reactor::Reactor;
use crate::wire::control::channel;

use super::assembly::spawn_http;
use super::Daemon;

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
