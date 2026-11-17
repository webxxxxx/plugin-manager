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
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::lan::Lan;
use crate::rawhub::RawHub;
use crate::server::http::hooks::{FallbackHandler, PreBodyRoutes};
use crate::server::http::Face;

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

/// Control-channel plumbing only a Unix server loop can own. The composition
/// root builds it from its own cfg matrix; on other hosts the value never
/// exists, so every field is compiled out.
pub(crate) struct ControlPlumbing {
    #[cfg(unix)]
    pub listener: std::os::unix::net::UnixListener,
    #[cfg(unix)]
    pub pending: std::sync::mpsc::SyncSender<crate::channel::PendingCall>,
    #[cfg(unix)]
    pub wake: Arc<crate::wake::Wake>,
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
        self.stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A driver the caller runs itself: the composition root supplies it because
/// it composes faces.
pub(crate) type ThreadDriver =
    Arc<dyn Fn(TcpListener, Arc<Mutex<Lan>>, PathBuf) -> ServerHandle + Send + Sync>;

/// Everything a driver needs, on every host. Platform-specific pieces are
/// `Option`; whichever driver consumes a value leaves the rest untouched.
pub(crate) struct StartInput {
    pub config: PathBuf,
    pub lan: Arc<Mutex<Lan>>,
    pub raw_hub: Arc<RawHub>,
    pub listener: Option<TcpListener>,
    pub face: Arc<dyn Face>,
    pub routes: Arc<dyn PreBodyRoutes>,
    pub fallback: FallbackHandler,
    /// Present on Unix only; the poll loop merges the control channel into its
    /// wait set. `None` elsewhere.
    pub control: Option<ControlPlumbing>,
    /// The per-connection driver, used when no poll loop runs.
    pub threads: ThreadDriver,
}
