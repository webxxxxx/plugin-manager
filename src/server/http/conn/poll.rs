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
            budget,
            bus,
            governor,
            control,
        } = input;
        let (control_user, control_admin, pending, wake) = control.into_parts();
        let loop_handle = crate::server::http::server::start(
            listener,
            control_user,
            control_admin,
            pending,
            wake,
            lan,
            config,
            raw_hub,
            face,
            routes,
            fallback,
            budget,
            bus,
            governor,
        )?;
        Ok(ServerHandle::poll(loop_handle))
    }
}
