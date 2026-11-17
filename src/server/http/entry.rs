//! The one place that decides which connection driver this host gets.
//!
//! The composition root builds a [`StartInput`] and calls [`start`]; nothing
//! above this module branches on the platform. Unix runs the single poll loop
//! that also owns the control channel; every other host runs the thread driver
//! the caller supplied (#139).

use std::io;
use std::sync::atomic::AtomicBool;

pub(crate) use crate::server::http::conn::{
    ControlPlumbing, ServerHandle, StartInput, ThreadDriver,
};

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
        Ok(ServerHandle::poll(loop_handle))
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
