use std::sync::mpsc::Receiver;
use std::sync::Arc;
#[cfg(test)]
use std::time::SystemTime;

use serde_json::Value;

#[cfg(not(feature = "http"))]
type Api = ();
use crate::face::uichannel::EventBus;
use crate::kernel::registry::Registry;
use crate::platform::config::Config;
#[cfg(test)]
use crate::platform::paths::Paths;
#[cfg(feature = "http")]
use crate::server::auth::lan::{Lan, LanStatus};
use crate::supervise::ops::factory::Notifier;
use crate::supervise::ops::host::Supervisor;
use crate::wire::control::channel;
#[cfg(feature = "http")]
use std::sync::Mutex;

pub use bootstrap::run;

pub use shutdown::{shutdown, shutdown_quiet};

mod assembly;
mod bootstrap;
mod driver;
mod plugins;
mod shutdown;
mod tools;

#[cfg(test)]
mod tests;

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

/// Ask the running daemon to shut down. The event loop checks this flag on
/// every wake, so a caller on any thread (the Windows SCM control handler,
/// for example) can stop it without touching daemon state.
#[cfg(windows)]
pub use crate::platform::host::signal::request_shutdown;

struct Daemon {
    /// The one derived runtime configuration: settings file, directory set and
    /// limits. Every directory this process touches comes from here.
    pub(crate) config: Arc<Config>,
    /// The supervision state and its loop-facing interface (L3).
    pub(crate) host: Supervisor,
    pub(crate) rx: Receiver<channel::PendingCall>,
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    pub(crate) api: Option<Arc<Api>>,
    pub(crate) shutdown: bool,
    pub(crate) catalog: crate::face::catalog::Catalog,
    /// The one UI event bus built by the composition root.
    pub(crate) bus: Arc<EventBus>,
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
                let root = crate::platform::test_support::temp_dir().join(format!(
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
        #[cfg(not(feature = "http"))]
        let _ = &bus;
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
            catalog: crate::face::catalog::Catalog::new(),
            bus,
        }
    }
}
