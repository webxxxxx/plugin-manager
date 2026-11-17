//! The single poll loop (Unix): listener, sockets and the control channel share
//! one wait set.
//!
//! The loop lives in `server/http/server.rs`; this adapter is only the seam
//! between [`super::ConnectionDriver`] and that loop (#140).

use std::io;

use crate::server::http::conn::{ConnectionDriver, ServerHandle, StartInput};

/// Unix connection driver: one poll loop for everything.
pub(crate) struct Driver;

impl ConnectionDriver for Driver {
    fn start(input: StartInput) -> io::Result<ServerHandle> {
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
}
