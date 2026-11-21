//! Per-connection thread driver: one thread per accepted socket, with the
//! admission caps applied before the thread exists.
//!
//! The accept loop, admission and framing live in `server/http/thread_loop.rs`;
//! this adapter is the seam between [`super::ConnectionDriver`] and that loop
//! (#140). Since #205 it also owns the thread's lifetime: the connection
//! handler arrives as data in [`StartInput`], so the composition root writes no
//! closure and this module names no face.

use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::platform::host::threads as host_threads;
use crate::platform::log;
use crate::server::http::conn::{ConnectionDriver, ServerHandle, StartInput};
use crate::server::http::thread_loop::{admission, serve, Connection};

/// Thread-per-connection driver.
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
            governor,
            ..
        } = input;
        let Some(listener) = listener else {
            return Err(io::Error::other("the thread driver needs a bound listener"));
        };
        let limits = config.limits.clone();
        let idle_timeout = limits.idle_timeout;
        let connection = Arc::new(Connection {
            face,
            routes,
            fallback,
            budget,
            raw_hub,
            governor,
        });
        let admission = admission(limits.max_connections, limits.max_connections_per_ip);
        let stop = Arc::new(AtomicBool::new(false));
        let listener_stop = stop.clone();
        let spawned = std::thread::Builder::new()
            .name("svcd-http".into())
            .stack_size(host_threads::io_stack(&limits))
            .spawn(move || {
                serve(
                    listener,
                    config,
                    lan,
                    admission,
                    idle_timeout,
                    listener_stop,
                    connection,
                )
            });
        if let Err(error) = spawned {
            log::svcd(&format!("svcd: HTTP listener thread failed: {}", error));
        }
        Ok(ServerHandle::thread(stop))
    }
}
