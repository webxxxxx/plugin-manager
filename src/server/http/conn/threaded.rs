//! Per-connection thread driver: one thread per accepted socket, with the
//! admission caps applied before the thread exists.
//!
//! The accept loop, admission and framing live in `server/http/threads.rs`;
//! this adapter is only the seam between [`super::ConnectionDriver`] and that
//! loop (#140). It ignores `control` and the poll-loop inputs: a thread driver
//! has no wait set to merge anything into.

use std::io;

use crate::server::http::conn::{ConnectionDriver, ServerHandle, StartInput};

/// Thread-per-connection driver.
pub(crate) struct Driver;

impl ConnectionDriver for Driver {
    fn start(input: StartInput) -> io::Result<ServerHandle> {
        let StartInput {
            config,
            lan,
            listener,
            threads,
            ..
        } = input;
        let Some(listener) = listener else {
            return Err(io::Error::other("the thread driver needs a bound listener"));
        };
        Ok(threads(listener, lan, config))
    }
}
