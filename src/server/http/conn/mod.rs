//! The connection-driver seam (S3, #140).
//!
//! A driver owns the listener, the accept loop and the lifetime of every
//! connection it accepted. Two implementations exist because hosts differ in
//! what they can wait on:
//!
//! - [`poll`] — one poll loop over the listener, the sockets and the control
//!   channel (Unix);
//! - [`threaded`] — one thread per accepted connection, with the admission
//!   caps applied before the thread exists (every other host).
//!
//! Everything a driver needs arrives in one [`StartInput`]; nothing above this
//! module branches on the platform, and adding a third driver later (IOCP)
//! means one more `impl ConnectionDriver` plus one line in `entry`.

use std::io;
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::platform::config::Config;
use crate::server::auth::lan::Lan;
use crate::server::http::hooks::{FallbackHandler, PreBodyRoutes};
use crate::server::http::Face;
use crate::wire::contract::rawhub::RawHub;

#[cfg(unix)]
pub(crate) mod poll;
#[cfg_attr(unix, allow(dead_code))]
pub(crate) mod threaded;

/// One connection driver.
///
/// Contract:
///
/// - the driver owns the listener, its accept loop and the per-connection
///   lifetime; the caller only keeps the returned [`ServerHandle`];
/// - `StartInput::control` is consumed by drivers that can merge the control
///   channel into their wait set; a driver that cannot must ignore it;
/// - `shutdown` semantics belong to the driver: the poll loop stops and drains
///   queued control replies, the thread driver only stops accepting (a thread
///   already blocked in `accept` ends with the process).
pub(crate) trait ConnectionDriver {
    /// Take the listener and serve connections until `ServerHandle::shutdown`.
    fn start(input: StartInput) -> io::Result<ServerHandle>;
}

/// Handle over the running driver.
pub(crate) struct ServerHandle {
    #[cfg(unix)]
    inner: Option<crate::server::control::handle::LoopHandle>,
    stop: Arc<AtomicBool>,
}

impl ServerHandle {
    /// Handle for a driver the caller runs itself (the thread driver).
    pub(crate) fn thread(stop: Arc<AtomicBool>) -> Self {
        Self {
            #[cfg(unix)]
            inner: None,
            stop,
        }
    }

    /// Handle over a poll loop that is already running.
    #[cfg(unix)]
    pub(crate) fn poll(inner: crate::server::control::handle::LoopHandle) -> Self {
        Self {
            inner: Some(inner),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ask the driver to shut down. Safe to call once.
    pub(crate) fn shutdown(&self) {
        #[cfg(unix)]
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
    /// The bound control channel. The poll loop merges its listener into the
    /// wait set; the thread driver ignores it because the channel already runs
    /// its own accept thread on that host.
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
        #[cfg(unix)]
        assert_driver::<poll::Driver>();
        #[cfg(not(unix))]
        assert_driver::<threaded::Driver>();
    }
}
