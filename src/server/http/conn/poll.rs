//! The one poll loop: listener, sockets and (on Unix) the control channel
//! share a single wait set.
//!
//! The loop lives in `server/http/server.rs`; this adapter is only the seam
//! between [`super::ConnectionDriver`] and that loop (#140). Unix waits with
//! `poll(2)`, Windows with `WSAPoll`; the connection code is the same.

use std::io;

use crate::server::http::conn::{ConnectionDriver, ServerHandle, StartInput};

/// The poll-loop driver.
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
            install_bus_wake,
            governor,
            control,
        } = input;
        #[cfg(unix)]
        let control = {
            let (user, admin, pending, wake) = control.into_parts();
            crate::server::http::server::ControlWiring {
                user,
                admin,
                pending,
                wake,
            }
        };
        #[cfg(not(unix))]
        let control = {
            // Windows runs the control channel on its own named-pipe accept
            // thread; the marker only has to cross this seam and be dropped.
            let _ = control;
            crate::server::http::server::ControlWiring
        };
        let loop_handle = crate::server::http::server::start(
            listener,
            control,
            lan,
            config,
            raw_hub,
            face,
            routes,
            fallback,
            budget,
            install_bus_wake,
            governor,
        )?;
        Ok(ServerHandle::poll(loop_handle))
    }
}
