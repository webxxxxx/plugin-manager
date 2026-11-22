//! The connection-driver seam (S3, #140).
//!
//! A driver owns the listener, the accept loop and the lifetime of every
//! connection it accepted. Two implementations exist because hosts differ in
//! what they can wait on:
//!
//! - [`poll`] — one poll loop over the listener and the sockets (Unix
//!   `poll(2)`, Windows `WSAPoll`); on Unix it also owns the control channel;
//! - [`threaded`] — one thread per accepted connection, with the admission
//!   caps applied before the thread exists (a host with no wait set).
//!
//! Everything a driver needs arrives in one [`StartInput`]; nothing above this
//! module branches on the platform, and adding another driver means one more
//! `impl ConnectionDriver` plus one line in `entry`.

use std::io;
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::platform::config::Config;
use crate::server::auth::lan::Lan;
use crate::server::http::hooks::{FallbackHandler, PreBodyRoutes};
use crate::server::http::Face;
use crate::wire::contract::rawhub::RawHub;

pub(crate) mod poll;
#[cfg_attr(not(windows), allow(dead_code))]
#[cfg_attr(any(unix, windows), allow(dead_code))]
pub(crate) mod threaded;

/// How the composition root hands the UI bus its wake-up hook: the installer
/// receives a closure the bus calls when it publishes. Naming it once keeps the
/// driver's input type readable and keeps this layer from naming the face.
pub type BusWakeInstall = Arc<dyn Fn(Arc<dyn Fn() + Send + Sync>) + Send + Sync>;

/// One connection driver.
///
/// Contract:
///
/// - the adapter owns the listener and returns a [`ServerHandle`]; the accept
///   loop and per-connection lifetime live in `server/http/thread_loop.rs`
///   (threaded) or `server/http/server.rs` (poll), not in the adapter;
/// - `StartInput::control` is consumed by drivers that can merge the control
///   channel into their wait set; a driver that cannot must ignore it;
/// - `shutdown` semantics belong to the loop: the poll loop stops and drains
///   queued control replies, the thread loop only stops accepting (a thread
///   already blocked in `accept` ends with the process).
pub(crate) trait ConnectionDriver {
    /// Take the listener and serve connections until `ServerHandle::shutdown`.
    fn start(input: StartInput) -> io::Result<ServerHandle>;
}

/// Handle over the running driver.
pub(crate) struct ServerHandle {
    inner: Option<crate::server::http::server::handle::LoopHandle>,
    stop: Arc<AtomicBool>,
}

impl ServerHandle {
    /// Handle for a driver the caller runs itself (the thread driver).
    #[cfg_attr(any(unix, windows), allow(dead_code))]
    pub(crate) fn thread(stop: Arc<AtomicBool>) -> Self {
        Self { inner: None, stop }
    }

    /// Handle over a poll loop that is already running.
    pub(crate) fn poll(inner: crate::server::http::server::handle::LoopHandle) -> Self {
        Self {
            inner: Some(inner),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ask the driver to shut down. Safe to call once.
    pub(crate) fn shutdown(&self) {
        if let Some(inner) = &self.inner {
            inner.shutdown();
        }
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Everything a driver needs, on every host. Platform-specific pieces are
/// `Option`; whichever driver consumes a value leaves the rest untouched.
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
    /// The CPU budget, when the document sets one. Both drivers refuse new
    /// connections while it is exceeded.
    pub governor: Option<Arc<crate::server::http::governor::CpuGovernor>>,
    /// The bound control channel. The poll loop merges its listener into the
    /// wait set; the thread driver ignores it because the channel already runs
    /// its own accept thread on that host.
    #[cfg_attr(not(unix), allow(dead_code))]
    // read by the Unix poll driver; the thread driver owns its own accept thread.
    pub control: crate::wire::control::channel::ControlBinding,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both drivers must satisfy the same seam; which one is compiled here
    /// depends on the host, so the assertion is per host and there is no
    /// third "test only" driver.
    fn assert_driver<D: ConnectionDriver>() {}

    #[test]
    fn the_host_driver_implements_the_seam() {
        #[cfg(any(unix, windows))]
        assert_driver::<poll::Driver>();
        #[cfg(not(any(unix, windows)))]
        assert_driver::<threaded::Driver>();
    }
}
