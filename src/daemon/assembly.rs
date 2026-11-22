#[cfg(feature = "http")]
use std::fs;
#[cfg(feature = "http")]
use std::net::{SocketAddr, TcpListener};
#[cfg(feature = "http")]
use std::sync::Arc;
#[cfg(feature = "http")]
use std::sync::Mutex;

#[cfg(feature = "http")]
use crate::face::fallback::respond;
#[cfg(feature = "http")]
use crate::face::prebody::PreBodyFaces;
#[cfg(feature = "http")]
use crate::face::uichannel::EventBus;
#[cfg(feature = "http")]
use crate::platform::config::Config;
#[cfg(feature = "http")]
#[cfg(feature = "http")]
use crate::server::auth::lan::Lan;

#[cfg(feature = "http")]
use super::Api;

#[cfg(feature = "http")]
pub(super) fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 8787))
}

#[cfg(feature = "http")]
pub(super) fn resolve_bind(env: Option<&str>, configured: Option<SocketAddr>) -> SocketAddr {
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
pub(super) fn spawn_http(
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
        warn!(
            "bind {} requires auth_passwd; falling back to 127.0.0.1:{}",
            requested,
            requested.port()
        );
        bind = SocketAddr::from(([127, 0, 0, 1], requested.port()));
    }
    let require_auth = !bind.ip().is_loopback();
    let mut state = Lan::new(bind, requested, require_auth, password_hash);
    // A failed bind disables HTTP only: the control channel still serves the
    // CLI, so the daemon stays manageable.
    let listener = match TcpListener::bind(bind) {
        Ok(listener) => match listener.local_addr() {
            Ok(local) => {
                state.bind = local;
                let port_file = config.paths.run_dir.join("http.port");
                if let Err(error) = fs::write(&port_file, format!("127.0.0.1:{}", local.port())) {
                    error!("cannot write {}: {}", port_file.display(), error);
                }
                info!("HTTP API on {} (auth required: {})", local, require_auth);
                Some(listener)
            }
            Err(error) => {
                warn!("HTTP API disabled: {}", error);
                None
            }
        },
        Err(error) => {
            warn!("HTTP API disabled: {}", error);
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

    // One CPU budget for this process: the kernel cap is installed once here,
    // and the governor is what both drivers consult when they refuse a new
    // connection. A budget of 0 installs neither.
    let cores = config
        .resources
        .profile
        .cpus
        .filter(|cores| *cores > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let governor =
        crate::server::http::governor::CpuGovernor::new(config.limits.cpu_budget_percent, cores);
    if let Some(governor) = &governor {
        let mode = match crate::platform::host::cpu::install_hard_cap(
            governor.target_percent(),
            governor.cores(),
        ) {
            Ok(installed) => installed,
            Err(why) => format!("admission shedding only ({why})"),
        };
        info!(
            "cpu: budget {}% of {} cores, {}",
            governor.target_percent(),
            governor.cores(),
            mode
        );
        governor.start();
    }

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
        install_bus_wake: Some(Arc::new(move |wake| bus.set_wake(wake))),
        governor,
    })?;
    Ok(Arc::new(Api { lan, driver }))
}

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
