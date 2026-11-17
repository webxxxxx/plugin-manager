//! The one place that decides which connection driver this host gets.
//!
//! The composition root builds a [`StartInput`] and calls [`start`]; nothing
//! above this module branches on the platform. Unix runs the single poll loop
//! that also owns the control channel; every other host runs the thread driver
//! the caller supplied (#139).

use std::io;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::lan::Lan;
use crate::rawhub::RawHub;
use crate::server::http::hooks::{FallbackHandler, PreBodyRoutes};
use crate::server::http::Face;

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

/// Handle over the running driver. Unix can stop the loop and drain its
/// control connections; the thread driver has no shared event source to wake,
/// so its stop flag only prevents new accepts — a thread already blocked in
/// `accept` stops with the process, which is what the fallback always did.
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

    /// Ask the driver to shut down. Safe to call once.
    pub(crate) fn shutdown(&self) {
        #[cfg(unix)]
        if let Some(inner) = &self.inner {
            inner.shutdown();
        }
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A driver the caller runs itself: the composition root supplies it because
/// it composes faces.
pub(crate) type ThreadDriver =
    Arc<dyn Fn(TcpListener, Arc<Mutex<Lan>>, PathBuf) -> ServerHandle + Send + Sync>;

/// Everything a driver needs, on every host. Platform-specific pieces are
/// `Option`; whichever branch consumes a value leaves the rest untouched.
pub(crate) struct StartInput {
    pub config: PathBuf,
    pub lan: Arc<Mutex<Lan>>,
    pub raw_hub: Arc<RawHub>,
    pub listener: Option<TcpListener>,
    pub face: Arc<dyn Face>,
    pub routes: Arc<dyn PreBodyRoutes>,
    pub fallback: FallbackHandler,
    /// Present on Unix only; the loop merges the control channel into its poll
    /// set. `None` elsewhere.
    pub control: Option<ControlPlumbing>,
    /// The thread-per-connection driver, used when no poll loop runs.
    pub threads: ThreadDriver,
}

/// Start this host's driver. This is the single platform branch.
pub(crate) fn start(input: StartInput) -> io::Result<ServerHandle> {
    let StartInput {
        config,
        lan,
        raw_hub,
        listener,
        face,
        routes,
        fallback,
        control,
        threads,
    } = input;

    #[cfg(unix)]
    {
        let _ = threads;
        let Some(control) = control else {
            return Err(io::Error::other(
                "the Unix server loop needs the control channel",
            ));
        };
        let loop_handle = crate::server::http::server::start(
            listener,
            control.listener,
            control.pending,
            control.wake,
            lan,
            config,
            raw_hub,
            face,
            routes,
            fallback,
        )?;
        Ok(ServerHandle {
            inner: Some(loop_handle),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    #[cfg(not(unix))]
    {
        let _ = (face, routes, fallback, raw_hub, control);
        let Some(listener) = listener else {
            return Err(io::Error::other(
                "the thread driver needs a bound listener",
            ));
        };
        Ok(threads(listener, lan, config))
    }
}
