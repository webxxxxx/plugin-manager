//! The connection-driver seam (S3, #140).
//!
//! A driver owns the listener, the accept loop and the lifetime of every
//! connection it accepted. One implementation exists: [`poll`], a single poll
//! loop over the listener and the sockets (Unix `poll(2)`, Windows `WSAPoll`),
//! which on Unix also owns the control channel. ADR-0027 removed the
//! per-connection thread driver: every host in the build matrix has a wait set.
//!
//! Everything a driver needs arrives in one [`StartInput`]; nothing above this
//! module branches on the platform.

use std::io;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use crate::platform::config::Config;
use crate::server::auth::lan::Lan;
use crate::server::http::hooks::{FallbackHandler, PreBodyRoutes};
use crate::server::http::Face;
use crate::wire::contract::rawhub::RawHub;

pub(crate) mod poll;

/// How the composition root hands the UI bus its wake-up hook: the installer
/// receives a closure the bus calls when it publishes. Naming it once keeps the
/// driver's input type readable and keeps this layer from naming the face.
pub type BusWakeInstall = Arc<dyn Fn(Arc<dyn Fn() + Send + Sync>) + Send + Sync>;

/// One connection driver.
///
/// Contract:
///
/// - the adapter owns the listener and returns a [`ServerHandle`]; the accept
///   loop and per-connection lifetime live in `server/http/server.rs`, not in
///   the adapter;
/// - `StartInput::control` is consumed by the loop that can merge the control
///   channel into its wait set; on Windows the named-pipe accept thread already
///   owns it, so the value only crosses the seam;
/// - `shutdown` semantics belong to the loop: it stops and drains queued
///   control replies before returning.
pub(crate) trait ConnectionDriver {
    /// Take the listener and serve connections until `ServerHandle::shutdown`.
    fn start(input: StartInput) -> io::Result<ServerHandle>;
}

/// Handle over the running driver.
pub(crate) struct ServerHandle {
    inner: crate::server::handle::LoopHandle,
}

impl ServerHandle {
    /// Handle over a poll loop that is already running.
    pub(crate) fn poll(inner: crate::server::handle::LoopHandle) -> Self {
        Self { inner }
    }

    /// Ask the loop to drain and return. Safe to call once.
    pub(crate) fn shutdown(&self) {
        self.inner.shutdown();
    }
}

/// Everything a driver needs, on every host. Platform-specific pieces are
/// `Option`; the loop leaves whatever it cannot use untouched.
pub(crate) struct StartInput {
    pub config: Arc<Config>,
    pub lan: Arc<Mutex<Lan>>,
    pub raw_hub: Arc<RawHub>,
    pub listener: Option<TcpListener>,
    pub face: Arc<dyn Face>,
    pub routes: Arc<dyn PreBodyRoutes>,
    pub fallback: FallbackHandler,
    /// The one long-lived stream budget built by the composition root.
    pub budget: crate::server::http::budget::StreamBudget,
    /// How the driver tells the UI bus where to write its wake-up. Passing an
    /// installer instead of the bus keeps this layer from naming the face
    /// module: the composition root owns the wire between the two.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub install_bus_wake: Option<BusWakeInstall>,
    /// The CPU budget, when the document sets one. The loop refuses new
    /// connections while it is exceeded.
    pub governor: Option<Arc<crate::server::http::governor::CpuGovernor>>,
    /// The bound control channel. The Unix loop merges its listener into the
    /// wait set; the Windows named-pipe accept thread already runs, so the
    /// value only crosses this seam.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub control: crate::wire::control::channel::ControlBinding,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_driver<D: ConnectionDriver>() {}

    #[test]
    fn the_host_driver_implements_the_seam() {
        assert_driver::<poll::Driver>();
    }
}
