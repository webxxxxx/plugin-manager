//! The one place that decides which connection driver this host gets.
//!
//! The composition root builds a [`StartInput`] and calls [`start`]; nothing
//! above this module branches on the platform. The drivers live in
//! `server/http/conn`: the Unix poll loop, or one thread per connection
//! elsewhere. Adding a third driver (IOCP) is one more `impl ConnectionDriver`
//! plus the alias below.

use std::io;

pub(crate) use crate::server::http::conn::{
    ServerHandle, StartInput, ThreadDriver,
};

/// The driver this host runs. This alias is the only platform choice left.
#[cfg(unix)]
type HostDriver = crate::server::http::conn::poll::Driver;
#[cfg(not(unix))]
type HostDriver = crate::server::http::conn::threaded::Driver;

/// Start this host's driver.
pub(crate) fn start(input: StartInput) -> io::Result<ServerHandle> {
    use crate::server::http::conn::ConnectionDriver;
    HostDriver::start(input)
}
